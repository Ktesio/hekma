//! Integration tests for `hekma calls` (story 19-4): a read-only open that
//! starts nothing and creates nothing, the `--json` document, paging with
//! the printed next-page command, `show`, and the exit classes (2 a
//! malformed flag, cursor, id or time; 1 an unknown id; 0 an empty result).
//!
//! Every OS: no agent process and no host; the records are seeded straight
//! into a calls segment.

mod helpers;

use std::path::Path;

use helpers::{host_pid, run_hekma_agent, run_hekma_agent_with_env, TestContext};

const RUN: &str = "run_01KAAAAAAAAAAAAAAAAAAAAAAA";

/// Seed one schema-2 segment: `(outcome, status, class)` per row, one
/// second apart, so the ids and the stored starts agree.
fn seed(base: &Path, rows: &[(&str, Option<i64>, &str)]) -> Vec<String> {
    let calls_dir = base.join("calls");
    std::fs::create_dir_all(&calls_dir).unwrap();
    let conn = rusqlite::Connection::open(calls_dir.join("2026-01-02.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calls (
             call_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, token_id TEXT,
             control_depth TEXT NOT NULL, binding TEXT, disposition TEXT NOT NULL,
             hold_end TEXT, outcome TEXT NOT NULL, status INTEGER,
             status_class TEXT NOT NULL, model TEXT, input_tokens INTEGER,
             output_tokens INTEGER, cached_tokens INTEGER, started_at TEXT NOT NULL,
             ended_at TEXT NOT NULL, latency_us INTEGER NOT NULL, upstream_us INTEGER);
         CREATE INDEX IF NOT EXISTS idx_calls_run_call ON calls(run_id, call_id);
         CREATE INDEX IF NOT EXISTS idx_calls_problem ON calls(call_id)
             WHERE disposition <> 'pass' OR outcome <> 'completed'
             OR status_class NOT IN ('2xx','3xx');
         PRAGMA user_version = 2;",
    )
    .unwrap();
    // 2026-01-02T00:00:00Z in Unix milliseconds.
    const BASE_MS: u64 = 1_767_312_000_000;
    let mut ids = Vec::new();
    for (index, (outcome, status, class)) in rows.iter().enumerate() {
        let id = format!(
            "call_{}",
            ulid_at(BASE_MS + 1000 + 1000 * index as u64, index as u8)
        );
        let started = format!("2026-01-02T00:00:{:02}.000000Z", index + 1);
        let disposition = if *outcome == "refused" {
            "block"
        } else {
            "pass"
        };
        conn.execute(
            "INSERT INTO calls VALUES (?1, ?2, NULL, 'wire', 'openai', ?3, NULL, ?4, ?5, ?6, \
             'gpt-demo', 10, NULL, 2, ?7, ?7, 1000, 900)",
            rusqlite::params![id, RUN, disposition, outcome, status, class, started],
        )
        .unwrap();
        ids.push(id);
    }
    ids
}

/// A ULID at `unix_millis` whose random half varies by `nonce`.
fn ulid_at(unix_millis: u64, nonce: u8) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut value: u128 = u128::from(unix_millis & ((1 << 48) - 1)) << 80;
    value |= u128::from(nonce as u64) << 40;
    (0..26)
        .rev()
        .map(|group| ALPHABET[((value >> (5 * group)) & 0x1F) as usize] as char)
        .collect()
}

/// Every database file under `root`, with its length (what a read must not
/// change). SQLite's `-wal` and `-shm` side files are excluded: a read-only
/// connection to a WAL database may create them empty.
fn tree(root: &Path) -> Vec<(std::path::PathBuf, u64)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if !path.to_string_lossy().ends_with("-shm")
                && !path.to_string_lossy().ends_with("-wal")
            {
                let len = std::fs::metadata(&path).unwrap().len();
                files.push((path, len));
            }
        }
    }
    files.sort();
    files
}

/// A fresh state directory with a store and nothing else.
fn state() -> TestContext {
    let state = TestContext::new();
    drop(hekma_engine::Engine::open(Some(state.project_dir.clone())).unwrap());
    state
}

#[test]
fn an_empty_store_lists_nothing_creates_nothing_and_starts_nothing() {
    let ctx = TestContext::new();
    let state = state();
    let dir = state.project_dir.as_path();
    let before = tree(dir);
    let run = run_hekma_agent(&["calls", "list"], &ctx.project_dir, dir);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stderr.contains("no calls match"), "{}", run.stderr);
    let run = run_hekma_agent(&["calls", "list", "--json"], &ctx.project_dir, dir);
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["items"].as_array().unwrap().len(), 0);
    assert!(json["next_cursor"].is_null());
    assert_eq!(tree(dir), before, "a read created or changed a file");
    assert!(host_pid(dir).is_none(), "a read started a host");
}

#[test]
fn pages_follow_the_printed_command_and_show_matches_the_listing() {
    let ctx = TestContext::new();
    let state = state();
    let dir = state.project_dir.as_path();
    let ids = seed(
        dir,
        &[
            ("completed", Some(200), "2xx"),
            ("refused", Some(402), "refused"),
            ("completed", Some(200), "2xx"),
        ],
    );

    let run = run_hekma_agent(
        &["calls", "list", "--limit", "2", "--json"],
        &ctx.project_dir,
        dir,
    );
    assert!(run.success, "{}", run.stderr);
    let first: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["call_id"], ids[2].as_str(), "newest first");
    assert_eq!(items[1]["call_id"], ids[1].as_str());
    assert!(
        items[0]["output_tokens"].is_null(),
        "unknown is null, never 0"
    );
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a next page")
        .to_string();
    let hint = format!("hekma calls list --limit 2 --cursor {cursor}");
    assert!(run.stderr.contains(&hint), "{}", run.stderr);

    let run = run_hekma_agent(
        &[
            "calls", "list", "--limit", "2", "--cursor", &cursor, "--json",
        ],
        &ctx.project_dir,
        dir,
    );
    assert!(run.success, "{}", run.stderr);
    let second: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    let items = second["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["call_id"], ids[0].as_str());
    assert!(second["next_cursor"].is_null());

    // The human table names every call; --problems keeps only the refusal.
    let run = run_hekma_agent_with_env(
        &["calls", "list"],
        &ctx.project_dir,
        dir,
        &[("COLUMNS", "132")],
    );
    assert!(run.success, "{}", run.stderr);
    for id in &ids {
        assert!(run.stdout.contains(id.as_str()), "{}", run.stdout);
    }
    assert!(run.stdout.contains("refused (block)"), "{}", run.stdout);
    let run = run_hekma_agent(
        &["calls", "list", "--problems", "--json"],
        &ctx.project_dir,
        dir,
    );
    let problems: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    let items = problems["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["call_id"], ids[1].as_str());

    // `show --json` is the listing's item, byte for byte.
    let run = run_hekma_agent(&["calls", "show", &ids[1], "--json"], &ctx.project_dir, dir);
    assert!(run.success, "{}", run.stderr);
    let shown: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(shown, problems["items"][0]);
    let run = run_hekma_agent(&["calls", "show", &ids[1]], &ctx.project_dir, dir);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stdout.contains("refused"), "{}", run.stdout);
    assert!(
        run.stdout.contains("output tokens  unknown"),
        "{}",
        run.stdout
    );
    assert!(host_pid(dir).is_none(), "a read started a host");
}

#[test]
fn refusals_land_on_their_exit_classes() {
    let ctx = TestContext::new();
    let state = state();
    let dir = state.project_dir.as_path();
    seed(dir, &[("completed", Some(200), "2xx")]);
    // 2: a malformed flag, cursor, id, Run or time.
    for (args, needle) in [
        (&["calls", "list", "--from", "yesterday"][..], "yesterday"),
        (
            &[
                "calls",
                "list",
                "--from",
                "2026-01-03T00:00:00Z",
                "--to",
                "2026-01-02T00:00:00Z",
            ][..],
            "",
        ),
        (&["calls", "list", "--outcome", "superseded"][..], "outcome"),
        (&["calls", "list", "--cursor", "not-a-cursor"][..], ""),
        (&["calls", "list", "--run", "nope"][..], "nope"),
        (&["calls", "show", "call_short"][..], "call_short"),
    ] {
        let run = run_hekma_agent(args, &ctx.project_dir, dir);
        assert_eq!(run.code, Some(2), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(needle), "{args:?}: {}", run.stderr);
    }
    // 1: a well-formed id no record has, named, with the way to list them.
    let ghost = format!("call_{}", ulid_at(1_767_312_999_000, 9));
    let run = run_hekma_agent(&["calls", "show", &ghost], &ctx.project_dir, dir);
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains(&ghost), "{}", run.stderr);
    assert!(run.stderr.contains("hekma calls list"), "{}", run.stderr);
    // A clamped page size notes it and still serves.
    let run = run_hekma_agent(&["calls", "list", "--limit", "5000"], &ctx.project_dir, dir);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stderr.contains("above the page cap"), "{}", run.stderr);
}
