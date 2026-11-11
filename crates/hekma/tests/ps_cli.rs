//! Integration tests for `hekma ps` (story 21-5): the exception-first
//! listing against a real state directory seeded with Runs in every shape
//! the views distinguish, through the real binary — output shape, `--json`,
//! the cursor pages, the refusals and exit codes, the widths, and the
//! read-only posture (an empty state dir creates nothing and starts no
//! host).
//!
//! Seeding is rows written directly into the state store (no processes —
//! Windows-safe): one engine open migrates the schema and creates the
//! Policies, SQL seeds the Run rows and labels, and a second open's
//! initial policy sweep resolves the open Runs so their headroom reads a
//! real stored resolution.

mod helpers;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hekma_engine::{Engine, PolicyChange, PolicyLimits};
use helpers::{run_hekma_agent, run_hekma_agent_with_env, TestContext};

/// `days` ago as the RFC 3339 shape the rows store (`YYYY-MM-DDTHH:MM:SSZ`,
/// whole seconds, `Z`) — computed through the engine's own public window
/// helper: `ended_run_cutoff(now + (7 − days) days)` lands exactly `days`
/// ago (the cutoff subtracts seven days).
fn days_ago(days: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    hekma_engine::ended_run_cutoff(now + 7 * 86_400 - days * 86_400)
}

/// The seeded fixture (the story's acceptance shape):
///
/// * `demo`'s owner Run `run_J` — `active` with the `breach` Anomaly and a
///   latched `block`, bound label `team=core` (the `tight` Policy's
///   Selector), 300 tokens of subtree usage;
/// * `run_H` — a held child of `run_J` with a caller label;
/// * `run_G` — ended with the `failed` Anomaly 2 days ago (inside the
///   window);
/// * `run_F` — ended clean 2 days ago;
/// * `run_E` — ended with an Anomaly 20 days ago (outside the window);
/// * `calm`'s owner Run `run_K` — `active`, healthy, no labels.
fn seed(state_dir: &Path) {
    {
        let engine = Engine::open(Some(state_dir.to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade
            .create_policy(
                PolicyChange {
                    name: "tight".to_string(),
                    selector: Some("team=core".parse().unwrap()),
                    set: PolicyLimits {
                        // Below the seeded 300-token usage, so the latched
                        // breach is a state the engine keeps (a ceiling
                        // above the usage would re-arm at every open).
                        tokens_per_run: Some(200),
                        ..PolicyLimits::default()
                    },
                    clear: Vec::new(),
                    rates: std::collections::BTreeMap::new(),
                },
                false,
            )
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, \
                 updated_at) VALUES ('demo', 'mock', 'running', '/h', 't', 't'); \
             INSERT INTO agent_instances (name, kind, state, agent_home, created_at, \
                 updated_at) VALUES ('calm', 'mock', 'running', '/h', 't', 't');",
        )
        .unwrap();
        let demo: i64 = conn
            .query_row(
                "SELECT id FROM agent_instances WHERE name = 'demo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let calm: i64 = conn
            .query_row(
                "SELECT id FROM agent_instances WHERE name = 'calm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let run = |id: &str,
                   instance: i64,
                   owner: bool,
                   status: &str,
                   anomalies: &str,
                   parent: Option<&str>,
                   ended: Option<&str>,
                   labels: &str| {
            format!(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 parent_run_id, instance_id, process_owner, end_cause, created_at, updated_at, \
                 ended_at) VALUES ('{id}', 'supervised', 0, '{status}', '{anomalies}', \
                 {parent}, {instance}, {owner_int}, NULL, 't', 't', {ended}); \
                 {labels}",
                owner_int = owner as i64,
                parent = parent
                    .map(|p| format!("'{p}'"))
                    .unwrap_or_else(|| "NULL".to_string()),
                ended = ended
                    .map(|e| format!("'{e}'"))
                    .unwrap_or_else(|| "NULL".to_string()),
            )
        };
        conn.execute_batch(&run(
            "run_J",
            demo,
            true,
            "active",
            "breach",
            None,
            None,
            &format!(
                "INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
                 ('run_J', 'team', 'core', 'bound', 'config'); \
                 INSERT INTO run_latches (run_id, dimension, scope, action, latched_at) VALUES \
                 ('run_J', 'tokens', 'per-run', 'block', 't'); \
                 INSERT INTO usage_rollup_run (instance_id, run_id, events, input_tokens, \
                 output_tokens, cached_tokens, cached_unknown, cost_micros) VALUES \
                 ({demo}, 'run_J', 1, 300, 0, 0, 0, 0); \
                 INSERT INTO usage_rollup_instance (instance_id, events, input_tokens, \
                 output_tokens, cached_tokens, cached_unknown, cost_micros) VALUES \
                 ({demo}, 1, 300, 0, 0, 0, 0);"
            ),
        ))
        .unwrap();
        conn.execute_batch(&run(
            "run_H",
            demo,
            false,
            "held",
            "",
            Some("run_J"),
            None,
            "INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
             ('run_H', 'flow', 'triage', 'caller', NULL);",
        ))
        .unwrap();
        conn.execute_batch(&run(
            "run_G",
            demo,
            true,
            "ended",
            "failed",
            None,
            Some(&days_ago(2)),
            "",
        ))
        .unwrap();
        conn.execute_batch(&run(
            "run_F",
            demo,
            true,
            "ended",
            "",
            None,
            Some(&days_ago(2)),
            "",
        ))
        .unwrap();
        conn.execute_batch(&run(
            "run_E",
            demo,
            true,
            "ended",
            "failed",
            None,
            Some(&days_ago(20)),
            "",
        ))
        .unwrap();
        conn.execute_batch(&run("run_K", calm, true, "active", "", None, None, ""))
            .unwrap();
    }
    // Reopen: the open policy sweep resolves the open Runs, so `run_J`
    // carries a real stored resolution sourced from `tight`.
    {
        let engine = Engine::open(Some(state_dir.to_path_buf())).unwrap();
        let _ = engine.blocking().policies();
    }
}

#[test]
fn ps_lists_the_exceptions_and_counts_the_healthy() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    // COLUMNS=132: the whole cells render (the 80-column default truncates
    // the ANOMALIES cell honestly — `--json` carries every figure whole).
    let ps = run_hekma_agent_with_env(&["ps"], &ctx.project_dir, state_dir, &[("COLUMNS", "132")]);
    assert!(ps.success, "stderr: {}", ps.stderr);
    // The three exceptions, newest first: the breached owner (by instance
    // name), the held child (by run id), the recent failed end (its owner
    // row renders the instance name too — `--json` carries the ids). The
    // healthy owner and the clean end are NOT listed (the counts prove
    // WHICH rows rendered).
    let demo_at = ps.stdout.find("demo").expect("the breached owner lists");
    let held_at = ps.stdout.find("run_H").expect("the held child lists");
    let ended_at = ps
        .stdout
        .lines()
        .position(|line| line.contains("ended"))
        .expect("the recent failed end lists");
    let demo_line = ps
        .stdout
        .lines()
        .position(|line| line.contains("demo"))
        .expect("the owner row");
    let held_line = ps
        .stdout
        .lines()
        .position(|line| line.contains("run_H"))
        .expect("the child row");
    assert!(
        demo_line < held_line && held_line < ended_at,
        "{}",
        ps.stdout
    );
    assert_eq!(
        ps.stdout
            .lines()
            .filter(|line| line.contains("ended"))
            .count(),
        1,
        "exactly one ended row: {}",
        ps.stdout
    );
    let _ = demo_at;
    let _ = held_at;
    // The latched action rides the Anomaly cell.
    assert!(ps.stdout.contains("breach[block]"), "{}", ps.stdout);
    // The footer names the counts.
    assert!(
        ps.stderr
            .contains("3 listed · 1 healthy open Run not shown (--all lists them)"),
        "{}",
        ps.stderr
    );
    // A read-only read: no host was started.
    assert!(!state_dir.join("control.json").exists());
}

#[test]
fn ps_all_lists_the_recent_clean_end_not_the_old_one() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    let ps = run_hekma_agent(&["ps", "--all"], &ctx.project_dir, state_dir);
    assert!(ps.success, "stderr: {}", ps.stderr);
    // Five rows — the three exceptions plus the healthy owner and the
    // recent clean end; the 20-day-old end stays outside the window (the
    // counts and the row title prove which rows rendered: the ended owner
    // rows carry the instance name, the ids ride --json).
    assert!(ps.stdout.contains("(5 rows)"), "{}", ps.stdout);
    assert!(
        ps.stdout.contains("calm"),
        "the healthy owner lists: {}",
        ps.stdout
    );
    assert!(ps.stdout.contains("run_H"), "{}", ps.stdout);
    assert_eq!(
        ps.stdout
            .lines()
            .filter(|line| line.contains("ended"))
            .count(),
        2,
        "the clean end joins, the old end stays out: {}",
        ps.stdout
    );
    assert!(
        ps.stderr.contains("5 listed · 1 healthy open Run"),
        "{}",
        ps.stderr
    );
}

#[test]
fn ps_json_carries_the_resolved_headroom_with_its_source() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    let ps = run_hekma_agent(&["ps", "--json"], &ctx.project_dir, state_dir);
    assert!(ps.success, "stderr: {}", ps.stderr);
    let document: serde_json::Value = serde_json::from_str(&ps.stdout).unwrap();
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["view"], "exceptions");
    assert_eq!(document["counts"]["listed"], 3);
    assert_eq!(document["counts"]["healthy_open"], 1);
    let items = document["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["run_id"], "run_J");
    assert_eq!(items[0]["status"], "active");
    assert_eq!(items[0]["anomalies"], serde_json::json!(["breach"]));
    assert_eq!(
        items[0]["latched"],
        serde_json::json!([{
            "dimension": "tokens",
            "scope": "per-run",
            "action": "block",
        }])
    );
    assert_eq!(items[0]["labels"][0]["key"], "team");
    assert_eq!(items[0]["labels"][0]["source"], "bound");
    assert_eq!(items[0]["usage"]["input_tokens"], 300);
    assert_eq!(items[0]["cumulative_usage"]["input_tokens"], 300);
    // Headroom against the POLICY-sourced ceiling (200 over 300 used,
    // saturating at zero), with the source named and the effective action
    // beside it.
    assert_eq!(items[0]["budget"]["per_run_limit"], 200);
    assert_eq!(items[0]["budget"]["per_run_remaining"], 0);
    assert_eq!(
        items[0]["budget"]["sources"]["tokens_per_run"],
        serde_json::json!({"policy": "tight"})
    );
    assert_eq!(items[0]["budget"]["effective_action"], "warn");
    // The held child: caller label, no cumulative block (absent, not zero).
    let held = &items[1];
    assert_eq!(held["run_id"], "run_H");
    assert_eq!(held["labels"][0]["source"], "caller");
    assert!(held.get("cumulative_usage").is_none(), "{held}");
}

#[test]
fn the_cli_and_the_engine_print_byte_shape_equal_items() {
    // FR-50 parity starts at the read model: `hekma ps --json`'s items are
    // byte-shape-equal to the engine's own document (the same struct the
    // Control API returns).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    let ps = run_hekma_agent(&["ps", "--json"], &ctx.project_dir, state_dir);
    assert!(ps.success, "stderr: {}", ps.stderr);
    let cli: serde_json::Value = serde_json::from_str(&ps.stdout).unwrap();

    let engine = Engine::open_read_only(Some(state_dir.to_path_buf())).unwrap();
    let listing = engine
        .blocking()
        .list_runs(hekma_engine::ListRunsQuery::exceptions())
        .unwrap();
    let reference: serde_json::Value =
        serde_json::to_value(listing.into_document(hekma_engine::ListingView::Exceptions, None))
            .unwrap();
    assert_eq!(cli, reference, "the CLI document equals the engine's");
}

#[test]
fn ps_pages_with_the_printed_cursor_disjointly() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    let page_one = run_hekma_agent(
        &["ps", "--all", "--limit", "2"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(page_one.success, "stderr: {}", page_one.stderr);
    let cursor = page_one
        .stderr
        .split("--cursor ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the footer prints the next-page cursor")
        .to_string();
    let page_two = run_hekma_agent(
        &["ps", "--all", "--limit", "2", "--cursor", &cursor],
        &ctx.project_dir,
        state_dir,
    );
    assert!(page_two.success, "stderr: {}", page_two.stderr);
    // Disjoint pages, newest first overall: calm's owner row then demo's,
    // then the held child and the ended owner rows.
    assert!(page_one.stdout.contains("calm"), "{}", page_one.stdout);
    assert!(page_one.stdout.contains("demo"), "{}", page_one.stdout);
    assert!(page_two.stdout.contains("run_H"), "{}", page_two.stdout);
    assert!(page_two.stdout.contains("ended"), "{}", page_two.stdout);
    assert!(!page_one.stdout.contains("run_H"), "{}", page_one.stdout);
    assert!(!page_two.stdout.contains("calm"), "{}", page_two.stdout);
    // A full page still points at the next one; the last, short page does
    // not (next_cursor is null exactly when the read is exhausted).
    assert!(
        page_two.stderr.contains("next page"),
        "a full page prints the next-page hint: {}",
        page_two.stderr
    );
    let cursor = page_two
        .stderr
        .split("--cursor ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("page two's cursor")
        .to_string();
    let page_three = run_hekma_agent(
        &["ps", "--all", "--limit", "2", "--cursor", &cursor],
        &ctx.project_dir,
        state_dir,
    );
    assert!(page_three.success, "stderr: {}", page_three.stderr);
    assert!(page_three.stdout.contains("ended"), "{}", page_three.stdout);
    assert!(
        !page_three.stderr.contains("next page"),
        "the short page ends the read: {}",
        page_three.stderr
    );
}

#[test]
fn ps_refusals_and_limits_map_to_the_documented_exit_codes() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    // 2 — a cursor that is not the codec's form.
    let ps = run_hekma_agent(&["ps", "--cursor", "!!!"], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(2), "stderr: {}", ps.stderr);
    assert!(ps.stderr.contains("cursor"), "{}", ps.stderr);
    // 2 — a malformed Selector, refused before the state directory opens.
    let ps = run_hekma_agent(
        &["ps", "--selector", "no-equals"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(ps.code, Some(2), "stderr: {}", ps.stderr);
    // 2 — a non-numeric limit (clap).
    let ps = run_hekma_agent(&["ps", "--limit", "abc"], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(2), "stderr: {}", ps.stderr);
    // A limit above the cap clamps with a note, exit 0.
    let ps = run_hekma_agent(&["ps", "--limit", "1001"], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(0), "stderr: {}", ps.stderr);
    assert!(
        ps.stderr.contains("above the page cap of 1000"),
        "{}",
        ps.stderr
    );
    // --limit 0 means the maximum.
    let ps = run_hekma_agent(&["ps", "--limit", "0"], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(0), "stderr: {}", ps.stderr);
    // A good cursor round-trips (the codec is opaque; page one of the all
    // view accepts it as the after key even though the row moved nowhere).
    let cursor = hekma_engine::encode_cursor("run_Z");
    let ps = run_hekma_agent(&["ps", "--cursor", &cursor], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(0), "stderr: {}", ps.stderr);
}

#[test]
fn ps_selector_filters_bound_and_caller_labels_in_either_view() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    // run_J carries team=core (bound); run_H carries nothing matching.
    let ps = run_hekma_agent(
        &["ps", "--selector", "team=core"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(ps.success, "stderr: {}", ps.stderr);
    assert!(ps.stdout.contains("demo"), "{}", ps.stdout);
    assert!(!ps.stdout.contains("run_H"), "{}", ps.stdout);
    assert!(ps.stderr.contains("1 listed"), "{}", ps.stderr);
}

#[test]
fn ps_on_an_empty_state_dir_prints_the_note_and_creates_nothing() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let ps = run_hekma_agent(&["ps"], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(0), "stderr: {}", ps.stderr);
    assert!(
        ps.stderr.contains("no Runs need attention"),
        "{}",
        ps.stderr
    );
    // Creating nothing: no state.db, no state directory contents, no host.
    assert!(
        !state_dir.join("state.db").exists(),
        "a read-only read creates nothing"
    );
    assert!(!state_dir.join("control.json").exists());

    // The empty --json document is a valid document.
    let ps = run_hekma_agent(&["ps", "--json"], &ctx.project_dir, state_dir);
    assert_eq!(ps.code, Some(0), "stderr: {}", ps.stderr);
    let document: serde_json::Value = serde_json::from_str(&ps.stdout).unwrap();
    assert_eq!(document["items"].as_array().unwrap().len(), 0);
    assert_eq!(document["counts"]["listed"], 0);
}

#[test]
fn ps_tables_fit_the_pinned_widths() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);
    for width in ["40", "80", "132"] {
        let ps =
            run_hekma_agent_with_env(&["ps"], &ctx.project_dir, state_dir, &[("COLUMNS", width)]);
        assert!(ps.success, "stderr: {}", ps.stderr);
        for line in ps.stdout.lines() {
            assert!(
                line.chars().count() <= width.parse::<usize>().unwrap(),
                "COLUMNS={width}: {line}"
            );
        }
        // Where the table fits (80/132), every header renders whole (a
        // column's cap is never below its header); at the 40-column floor
        // the shrink pass may clip headers — the rows still render and fit.
        if width != "40" {
            assert!(
                ps.stdout.contains("CONTROL"),
                "COLUMNS={width}: {}",
                ps.stdout
            );
            assert!(
                ps.stdout.contains("STATUS"),
                "COLUMNS={width}: {}",
                ps.stdout
            );
        }
        assert!(ps.stdout.contains("demo"), "COLUMNS={width}: {}", ps.stdout);
    }
}

#[test]
fn a_lowered_policy_re_resolves_and_ps_shows_the_new_headroom() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);

    // The 21-3 change job: raise the ceiling (still above the seeded
    // usage, so the re-arm pass clears the synthetic latch and the breach
    // Anomaly — the honest change semantics; the FRESH headroom is the
    // point here).
    {
        let engine = Engine::open(Some(state_dir.to_path_buf())).unwrap();
        engine
            .blocking()
            .change_policy(
                PolicyChange {
                    name: "tight".to_string(),
                    selector: None,
                    set: PolicyLimits {
                        tokens_per_run: Some(500),
                        ..PolicyLimits::default()
                    },
                    clear: Vec::new(),
                    rates: std::collections::BTreeMap::new(),
                },
                false,
            )
            .unwrap();
    }
    let ps = run_hekma_agent(
        &["ps", "--json", "--all", "--selector", "team=core"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(ps.success, "stderr: {}", ps.stderr);
    let document: serde_json::Value = serde_json::from_str(&ps.stdout).unwrap();
    let item = &document["items"][0];
    assert_eq!(item["run_id"], "run_J", "items: {}", document["items"]);
    // Never a stale or configured-only figure: the new resolution (500)
    // minus the same subtree usage.
    assert_eq!(
        item["budget"]["per_run_limit"], 500,
        "items: {}",
        document["items"]
    );
    assert_eq!(
        item["budget"]["per_run_remaining"], 200,
        "items: {}",
        document["items"]
    );
    // The re-arm cleared the synthetic breach (the raised ceiling no longer
    // holds a latch) — an honest row, never a stale Anomaly.
    assert_eq!(
        item["anomalies"],
        serde_json::json!([]),
        "items: {}",
        document["items"]
    );
}

#[test]
fn an_unpriced_policy_cap_shows_the_anomaly_and_no_dollar_figure() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);
    // A second Policy with a Cost Cap and NO Rate, matching a fresh Run.
    {
        let engine = Engine::open(Some(state_dir.to_path_buf())).unwrap();
        engine
            .blocking()
            .create_policy(
                PolicyChange {
                    name: "unpriced".to_string(),
                    selector: Some("team=unpriced".parse().unwrap()),
                    set: PolicyLimits {
                        dollars_per_run_micros: Some(5_000_000),
                        ..PolicyLimits::default()
                    },
                    clear: Vec::new(),
                    rates: std::collections::BTreeMap::new(),
                },
                false,
            )
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             instance_id, process_owner, created_at, updated_at) \
             SELECT 'run_U', 'supervised', 0, 'active', '', id, 0, 't', 't' \
             FROM agent_instances WHERE name = 'demo'; \
             INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
             ('run_U', 'team', 'unpriced', 'bound', 'config');",
        )
        .unwrap();
    }
    // The reopen sweep resolves run_U against the unpriced cap.
    {
        let engine = Engine::open(Some(state_dir.to_path_buf())).unwrap();
        let _ = engine.blocking().policies();
    }
    let ps = run_hekma_agent(
        &["ps", "--json", "--selector", "team=unpriced"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(ps.success, "stderr: {}", ps.stderr);
    let document: serde_json::Value = serde_json::from_str(&ps.stdout).unwrap();
    let item = &document["items"][0];
    assert_eq!(item["run_id"], "run_U");
    assert_eq!(item["anomalies"], serde_json::json!(["unpriced-model"]));
    // Never a fabricated $0.00: no dollar headroom at all.
    assert!(
        item["budget"].get("per_run_cost_cap").is_none()
            && item["budget"].get("per_run_dollars_remaining").is_none(),
        "{item}"
    );
}

#[test]
fn ps_help_names_the_flags() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let ps = run_hekma_agent(
        &["ps", "--help"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert!(ps.success, "stderr: {}", ps.stderr);
    for flag in ["--selector", "--all", "--limit", "--cursor", "--json"] {
        assert!(ps.stdout.contains(flag), "{flag} missing: {}", ps.stdout);
    }
}
