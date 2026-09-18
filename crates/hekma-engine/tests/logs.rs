//! Integration tests for story-4.2 "attach to live output and read retained
//! logs" (FR-25, NFR-5, spine AD-12), driven through the engine's PUBLIC
//! async API and blocking facade only (spine AD-2/AD-13), spawning the REAL
//! `fake_agent` helper so capture, attribution, and rotation are genuinely
//! exercised end to end.
//!
//! The story's non-negotiable constraint — `agent.log` (the legacy, raw,
//! unattributed combined stdout+stderr capture) MUST stay byte-identical to
//! its pre-story format, since Epic 3's `Supervisor::drain_usage_for`
//! (adversarially-reviewed billing-critical metering ingestion) parses it
//! today — is proven by `legacy_agent_log_is_byte_identical_to_pre_story_content`,
//! the automated regression guard CRITICAL SCOPING #3 requires.
//!
//! The dispatch is proven across:
//! * **AC-A/AC-G** — retained logs are ordered (append order, NEVER a
//!   timestamp re-sort), timestamped, and attributed.
//! * **AC-B** — `--follow`'s poll loop observes new lines INCREMENTALLY, not
//!   merely present in a final snapshot.
//! * **AC-C** — follow drains and exits cleanly (never hangs) on stop AND on
//!   pause, with state-appropriate honesty.
//! * **AC-E** — capture is unconditional, independent of declared
//!   `interaction` support (contrast with 4.1's `send`, which IS gated).
//! * **AC-H** — the genuinely novel edge case: an instance ADOPTED from a
//!   prior engine session (AD-5) can still be read/followed from a FRESH
//!   session — the mirror image of 4.1's AC-D (there, `send_input` fails;
//!   here, by design, it succeeds).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, Capability, Engine, LifecycleState, LogLine, LogStream, OsId, SupportLevel,
};
use tempfile::TempDir;

/// The current-OS key for a manifest `[capabilities.*]` table, as the wire
/// string the manifest uses (`linux`/`macos`/`windows`). Runtime data, not cfg.
fn current_os_key() -> &'static str {
    match OsId::current() {
        OsId::Linux => "linux",
        OsId::Macos => "macos",
        OsId::Windows => "windows",
        OsId::Other => "other",
    }
}

/// Write a manifest whose `[lifecycle.start]` exec points at `fake_agent`
/// with `args`, declaring BOTH `[capabilities.interaction]` and
/// `[capabilities.pause]` at `level` for the CURRENT OS (uniform across the
/// tests that don't specifically care about the per-OS honesty dispatch —
/// mirrors `interaction.rs`/`pause.rs`'s fixture manifests).
fn write_logs_manifest(dir: &Path, kind: &str, args: &[&str], level: &str) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "{kind}"

[lifecycle.start]
exec = {exec:?}
args = [{args_toml}]

[capabilities.interaction]
{os} = "{level}"

[capabilities.pause]
{os} = "{level}"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
        os = current_os_key(),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Open an engine over a fresh temp state dir.
fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// The legacy (raw, unattributed) agent-output log path inside an instance's
/// Agent Home.
fn agent_log_path(base: &Path, name: &str) -> PathBuf {
    base.join("agents")
        .join(name)
        .join("logs")
        .join("agent.log")
}

/// Poll `read_agent_log` until at least `min_out` `agent-out` lines and
/// `min_err` `agent-err` lines are retained (committed, observable state —
/// never a wall-clock sleep, per the Epic-2-retro AI-35/38 lesson every
/// later story internalizes). Returns the full retained set once satisfied.
fn wait_for_min_lines_per_stream(
    facade: &hekma_engine::Blocking<'_>,
    name: &str,
    min_out: usize,
    min_err: usize,
) -> Vec<LogLine> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (lines, _cursor) = facade.read_agent_log(name).expect("read_agent_log");
        let out = lines
            .iter()
            .filter(|l| l.stream == LogStream::AgentOut)
            .count();
        let err = lines
            .iter()
            .filter(|l| l.stream == LogStream::AgentErr)
            .count();
        if out >= min_out && err >= min_err {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "never observed enough lines per stream (want out>={min_out} err>={min_err}, have out={out} err={err})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Assert the numbered lines of `stream` whose text starts with `prefix`
/// (e.g. `"heartbeat "` -> `0, 1, 2, ...`) appear in STRICTLY INCREASING
/// numeric order across `lines` — the append-order fidelity proof (AC-G):
/// since the counter is monotonic at the SOURCE, observing it monotonic in
/// the READ order proves the read preserved emission order (a re-sort by a
/// whole-second timestamp, where many lines share the same `at`, would not
/// reliably preserve this).
fn assert_monotonic_counter(lines: &[LogLine], stream: LogStream, prefix: &str) {
    let mut last: Option<u64> = None;
    for l in lines.iter().filter(|l| l.stream == stream) {
        let Some(n) = l
            .text
            .strip_prefix(prefix)
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(prev) = last {
            assert!(
                n > prev,
                "counter must be strictly increasing in read/append order: {prev} then {n} ({lines:?})"
            );
        }
        last = Some(n);
    }
    assert!(
        last.is_some(),
        "expected at least one {prefix:?} line: {lines:?}"
    );
}

#[test]
fn retained_logs_are_ordered_timestamped_and_attributed() {
    // AC-A, AC-G: one Engine::open, start a manifest instance emitting on
    // BOTH streams via --heartbeat-ms/--heartbeat-stderr-ms, let several of
    // each accrue, call read_agent_log, and assert: every line has a
    // well-formed RFC3339 `at`, the attribution set is exactly
    // {agent-out, agent-err, engine} (at least one `engine` line from the
    // `start` transition), and order matches emission order.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_logs_manifest(
        manifest.path(),
        "svc",
        &[
            "--heartbeat-ms",
            "30",
            "--heartbeat-stderr-ms",
            "30",
            "--linger-ms",
            "600000",
        ],
        "guaranteed",
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();

    let lines = wait_for_min_lines_per_stream(&facade, "svc", 5, 5);

    for l in &lines {
        assert_eq!(l.at.len(), 20, "RFC3339 whole-second shape: {:?}", l.at);
        assert!(l.at.ends_with('Z'), "{:?}", l.at);
        assert_eq!(l.instance, "svc");
    }

    let streams: std::collections::HashSet<LogStream> = lines.iter().map(|l| l.stream).collect();
    assert_eq!(
        streams,
        [LogStream::AgentOut, LogStream::AgentErr, LogStream::Engine]
            .into_iter()
            .collect(),
        "attribution set must be exactly {{agent-out, agent-err, engine}}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.stream == LogStream::Engine && l.text.contains("-> running")),
        "an engine line from the start transition must be present: {lines:?}"
    );

    assert_monotonic_counter(&lines, LogStream::AgentOut, "heartbeat ");
    assert_monotonic_counter(&lines, LogStream::AgentErr, "stderr-heartbeat ");

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn follow_observes_new_lines_incrementally_not_just_a_final_snapshot() {
    // AC-B: a background OS thread calls read_agent_log_since in a poll
    // loop while the main test thread drives more heartbeats — assert the
    // follower's observed line count increases across MULTIPLE polls
    // (genuinely incremental), not merely present in one final read.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_logs_manifest(
        manifest.path(),
        "svc",
        &["--heartbeat-ms", "20", "--linger-ms", "600000"],
        "guaranteed",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();

    let stop_flag = AtomicBool::new(false);
    let growth_snapshots: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut cursor = 0u64;
            let mut total = 0usize;
            let deadline = Instant::now() + Duration::from_secs(20);
            while !stop_flag.load(Ordering::Relaxed) {
                let (new_lines, next_cursor) = facade
                    .read_agent_log_since("svc", cursor)
                    .expect("read_agent_log_since");
                if !new_lines.is_empty() {
                    total += new_lines.len();
                    growth_snapshots.lock().unwrap().push(total);
                }
                cursor = next_cursor;
                assert!(Instant::now() < deadline, "follower must not hang");
                std::thread::sleep(Duration::from_millis(15));
            }
        });

        // Drive the accrual in explicit ROUNDS, checking after EACH round
        // whether the follower observed FURTHER growth since the previous
        // round — ties the "genuinely incremental" proof to checkpoints the
        // MAIN thread controls, rather than to the follower thread's
        // independent OS-scheduling luck (robust under CI / parallel-test
        // CPU contention, where a single early snapshot could otherwise
        // coincidentally catch everything accrued so far in one batch).
        let mut last_seen = 0usize;
        let mut rounds_with_growth = 0;
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(200)); // ~10 heartbeats/round @ 20ms
            let seen = growth_snapshots
                .lock()
                .unwrap()
                .last()
                .copied()
                .unwrap_or(0);
            if seen > last_seen {
                rounds_with_growth += 1;
                last_seen = seen;
            }
            if rounds_with_growth >= 2 {
                break;
            }
        }
        stop_flag.store(true, Ordering::Relaxed);
        assert!(
            rounds_with_growth >= 2,
            "the follower must observe growth across MULTIPLE distinct rounds, not one final \
             snapshot (rounds_with_growth={rounds_with_growth})"
        );
    });

    let snapshots = growth_snapshots.into_inner().unwrap();
    assert!(
        snapshots.len() >= 2,
        "the follower must observe INCREMENTAL growth across multiple polls, not one final \
         snapshot: {snapshots:?}"
    );
    for w in snapshots.windows(2) {
        assert!(
            w[1] > w[0],
            "each growing poll must see MORE than before: {snapshots:?}"
        );
    }

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn follow_drains_and_exits_cleanly_on_stop() {
    // AC-C: stop the instance mid-follow; assert the follower's final poll
    // includes every line emitted up to the stop's `engine`-attributed
    // line, with no further growth after.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_logs_manifest(
        manifest.path(),
        "svc",
        &["--heartbeat-ms", "20", "--linger-ms", "600000"],
        "guaranteed",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    wait_for_min_lines_per_stream(&facade, "svc", 3, 0);

    let saw_stop_line = AtomicBool::new(false);
    let final_cursor: Mutex<u64> = Mutex::new(0);

    std::thread::scope(|scope| {
        let follower = scope.spawn(|| {
            let mut cursor = 0u64;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let (new_lines, next_cursor) = facade
                    .read_agent_log_since("svc", cursor)
                    .expect("read_agent_log_since");
                cursor = next_cursor;
                if new_lines
                    .iter()
                    .any(|l| l.stream == LogStream::Engine && l.text.contains("-> stopped"))
                {
                    saw_stop_line.store(true, Ordering::Relaxed);
                }
                let status = facade.instance_status("svc").expect("instance_status");
                if status.instance.state != LifecycleState::Running {
                    // One final drain (AC-C): no line up to the transition
                    // may be lost. Fix pass (review of #80): the
                    // engine-attributed "stopped" line is now written
                    // SYNCHRONOUSLY inside `stop_inner` (no background
                    // writer thread/channel involved at all — see
                    // `LogCapture::send_engine_line`'s docs) — but
                    // `stop_inner` persists the NEW STATE to the DB
                    // (`registry.set_state`, what `instance_status` above
                    // just read) BEFORE it writes that engine line: two
                    // separate steps within the SAME call, not one atomic
                    // unit. This poll runs in the SAME process/thread here,
                    // so in practice it rarely wins that narrow gap, but
                    // it is not IMPOSSIBLE (and the equivalent CLI-level
                    // race — a genuinely SEPARATE `kt agent logs --follow`
                    // process racing a concurrent `kt agent stop` — is
                    // real and is what M2's fix pass addresses in
                    // `kt/src/cli/agent.rs`). Retry the drain briefly
                    // (committed-state polling, never a fixed sleep) rather
                    // than a single one-shot read, so a slow scheduler
                    // moment cannot turn into a false test failure; the
                    // PRODUCT's own `kt agent logs --follow` performs the
                    // SAME bounded retry (M2), not a single unretried
                    // drain — this loop mirrors it.
                    let final_deadline = Instant::now() + Duration::from_secs(3);
                    loop {
                        let (more, final_c) = facade
                            .read_agent_log_since("svc", cursor)
                            .expect("final drain");
                        cursor = final_c;
                        if more
                            .iter()
                            .any(|l| l.stream == LogStream::Engine && l.text.contains("-> stopped"))
                        {
                            saw_stop_line.store(true, Ordering::Relaxed);
                        }
                        if saw_stop_line.load(Ordering::Relaxed) || Instant::now() >= final_deadline
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    *final_cursor.lock().unwrap() = cursor;
                    return;
                }
                assert!(Instant::now() < deadline, "follower must not hang");
                std::thread::sleep(Duration::from_millis(15));
            }
        });

        // Give the follower a head start so it is genuinely mid-poll, then stop.
        std::thread::sleep(Duration::from_millis(60));
        facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
        follower.join().expect("follower thread must not panic");
    });

    assert!(
        saw_stop_line.load(Ordering::Relaxed),
        "the follower must observe the stop's engine-attributed line"
    );

    // No further growth after the follower's own final drain — the
    // instance is stopped, so nothing new can ever arrive.
    let cursor = *final_cursor.lock().unwrap();
    let (more, _c) = facade.read_agent_log_since("svc", cursor).unwrap();
    assert!(
        more.is_empty(),
        "no growth may follow the final drain: {more:?}"
    );
}

#[test]
fn follow_on_a_paused_instance_notes_paused_not_hang() {
    // AC-C, the pause edge case: pause instead of stop; assert the follower
    // returns a clean "paused" signal rather than blocking forever waiting
    // for output that will not arrive while suspended.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_logs_manifest(
        manifest.path(),
        "svc",
        &["--heartbeat-ms", "20", "--linger-ms", "600000"],
        "guaranteed",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    wait_for_min_lines_per_stream(&facade, "svc", 2, 0);

    let observed_state: Mutex<Option<LifecycleState>> = Mutex::new(None);

    std::thread::scope(|scope| {
        let follower = scope.spawn(|| {
            let mut cursor = 0u64;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let (_new_lines, next_cursor) = facade
                    .read_agent_log_since("svc", cursor)
                    .expect("read_agent_log_since");
                cursor = next_cursor;
                let status = facade.instance_status("svc").expect("instance_status");
                if status.instance.state != LifecycleState::Running {
                    *observed_state.lock().unwrap() = Some(status.instance.state);
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "follow must exit honestly on pause, never hang"
                );
                std::thread::sleep(Duration::from_millis(15));
            }
        });

        std::thread::sleep(Duration::from_millis(60));
        facade.pause("svc").unwrap();
        follower.join().expect("follower thread must not panic");
    });

    assert_eq!(
        *observed_state.lock().unwrap(),
        Some(LifecycleState::Paused),
        "follow must cleanly observe the paused state, not hang or misreport it"
    );

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn output_capture_is_unconditional_regardless_of_declared_interaction_support() {
    // AC-E: a manifest declaring `interaction: unsupported` (on the current
    // OS); assert its stdout is still captured, attributed, and readable —
    // contrast directly with 4.1's
    // `send_input_on_unsupported_interaction_fails_fast_with_no_io`: reading
    // FROM the process is never capability-gated; only writing TO it
    // (`send`) is.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let other_os = match OsId::current() {
        OsId::Linux => "windows",
        OsId::Macos => "windows",
        OsId::Windows => "linux",
        OsId::Other => "linux",
    };
    let bin = hekma_conformance::fake_agent_bin();
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "unsup"

[lifecycle.start]
exec = {exec:?}
args = ["--heartbeat-ms", "30", "--linger-ms", "600000"]

[capabilities.interaction]
{other} = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
        other = other_os,
    );
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "unsup",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let caps = facade.effective_capabilities("unsup").unwrap();
    let level = caps
        .entries
        .iter()
        .find(|(c, _)| *c == Capability::Interaction)
        .map(|(_, l)| *l);
    assert_eq!(
        level,
        Some(SupportLevel::Unsupported),
        "sanity: this test must honestly exercise the unsupported path"
    );

    facade.start("unsup").unwrap();
    let lines = wait_for_min_lines_per_stream(&facade, "unsup", 2, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.stream == LogStream::AgentOut && l.text.starts_with("heartbeat ")),
        "output must be captured and attributed regardless of declared interaction support: {lines:?}"
    );

    facade.stop("unsup", Some(Duration::from_secs(5))).unwrap();
}

// ---- CRITICAL SCOPING #3: the non-negotiable agent.log byte-identity guard ----

/// The token sentinels `fake_agent --emit-usage` stamps on every event
/// (mirrors `metering.rs`'s fixture — the SAME convention, reused verbatim).
const USAGE_INPUT: u64 = 10;
const USAGE_OUTPUT: u64 = 20;

/// Poll the committed Usage-Ledger event count for `name` until it reaches
/// `expected` (mirrors `metering.rs`'s `wait_for_usage_rows` exactly — a
/// direct read-only connection to the same state DB the engine commits to,
/// deterministic committed state, never a wall-clock guess).
fn wait_for_usage_rows(state_dir: &Path, name: &str, expected: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let count = usage_row_count(state_dir, name);
        if count >= expected {
            return count;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} committed usage rows for '{name}' (have {count})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll `agent.log` until one of its lines satisfies `predicate`, returning the
/// WHOLE file once it does.
///
/// The counterpart of `wait_for_usage_rows` for the log file rather than the
/// ledger. `agent.log` is written by a LIVE child process, so "the line is not
/// there yet" is not "the line will never be there" — a bare `read_to_string`
/// races whatever the child has not flushed. Synchronising on the ledger does
/// NOT synchronise this file: they are independent streams.
fn wait_for_agent_log_line(
    agent_log: &Path,
    what: &str,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let contents = std::fs::read_to_string(agent_log).unwrap_or_default();
        if contents.lines().any(&predicate) {
            return contents;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} in agent.log: {contents:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn usage_row_count(state_dir: &Path, name: &str) -> u64 {
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).expect("open state db");
    conn.query_row(
        "SELECT COUNT(*) FROM usage_events e \
         JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

#[test]
fn legacy_agent_log_is_byte_identical_to_pre_story_content() {
    // CRITICAL SCOPING #3 — the automated regression guard this story is
    // built around: `Supervisor::drain_usage_for` (story 3-1/AD-7,
    // adversarially-reviewed billing-critical metering ingestion) reads
    // `agent.log` TODAY, splitting on '\n' and matching a
    // "KTESIO_USAGE {json}" sentinel AT THE START of each RAW physical
    // line. Prove BOTH halves hold: (a) `agent.log`'s bytes are UNCHANGED —
    // the exact raw lines `fake_agent` wrote, byte for byte, no JSON
    // envelope, no attribution prefix, no timestamp; and (b)
    // `drain_usage_for`'s ledger ingestion still works completely
    // unmodified end to end (mirrors `metering.rs`'s fixture pattern
    // exactly, reused verbatim — this story provably does not touch Epic
    // 3's billing path).
    //
    // Fix pass (review of #80): `agent.log`'s CAPTURE PATH changed TWICE
    // now — story 4-2 originally made it piped + engine-side reader
    // threads (never a kernel passthrough); THIS fix pass reverted it back
    // to a DIRECT, synchronous OS redirect (`Stdio::from(file)`) for the
    // child's stdout alone, closing an engine-crash-kills-the-agent
    // regression the piped design introduced (see
    // `ports::process_backend`'s module docs). `agent.log`'s CONTENT
    // guarantee below is unaffected either way — this test is precisely
    // what proves that across both capture-path reworks.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_logs_manifest(
        manifest.path(),
        "svc",
        &[
            "--heartbeat-ms",
            "40",
            "--emit-usage",
            "3",
            "--linger-ms",
            "600000",
        ],
        "guaranteed",
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // (b) The ledger ingestion is unaffected: exactly 3 committed rows, the
    // Fleet-detail totals equal the ledger exactly — the SAME assertion
    // shape story 3-1's own test makes.
    let count = wait_for_usage_rows(state.path(), "svc", 3);
    assert_eq!(count, 3, "exactly 3 usage rows, unaffected by this story");
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "svc").unwrap();
    assert_eq!(entry.usage.cumulative_input_tokens, 3 * USAGE_INPUT);
    assert_eq!(entry.usage.cumulative_output_tokens, 3 * USAGE_OUTPUT);
    assert_eq!(entry.metering_source, "self-reported");

    // (a) agent.log byte-identity: the exact raw lines, unmodified — no
    // JSON envelope, no attribution token, no timestamp prefix anywhere.
    let agent_log = agent_log_path(state.path(), "svc");
    // WAIT for the heartbeat rather than reading agent.log straight through.
    // `fake_agent` writes the ready line, then EVERY usage line, and only THEN
    // arms its heartbeat clock — so `heartbeat 0` lands one full --heartbeat-ms
    // interval (40ms here) AFTER the last usage line. `wait_for_usage_rows`
    // above synchronises on committed LEDGER rows, and the engine's drain pass
    // can commit all three inside that 40ms window, so a bare read here raced
    // the first beat: observed RED on macos-latest CI and GREEN on re-run with
    // no code change (PR #48). The beat is the LAST of the three line kinds
    // asserted below, so once it lands the ready + usage lines are necessarily
    // already on disk — waiting for it settles the whole file.
    let contents = wait_for_agent_log_line(
        &agent_log,
        "a heartbeat line, verbatim and unwrapped",
        |l| l == "heartbeat 0",
    );
    assert!(
        contents
            .lines()
            .any(|l| l.starts_with("fake_agent ready pid=")),
        "the raw ready line must appear verbatim: {contents:?}"
    );
    assert!(
        !contents.contains("schema_version"),
        "agent.log must carry NO JSON envelope: {contents:?}"
    );
    assert!(
        !contents.contains("\"stream\""),
        "agent.log must carry NO attribution field: {contents:?}"
    );
    assert!(
        !contents.contains("agent-out") && !contents.contains("agent-err"),
        "agent.log must carry NO attribution token: {contents:?}"
    );
    // The exact KTESIO_USAGE sentinel lines fake_agent emits, verbatim — the
    // literal string drain_usage_for's parser matches at line-start.
    for seq in 0..3u64 {
        let want =
            format!("KTESIO_USAGE {{\"sequence\":{seq},\"input_tokens\":{USAGE_INPUT},\"output_tokens\":{USAGE_OUTPUT}}}");
        assert!(
            contents.lines().any(|l| l == want),
            "sentinel line {seq} must appear BYTE-IDENTICAL (not wrapped/reformatted): {contents:?}"
        );
    }

    // The NEW attributed capture exists SEPARATELY and carries the SAME
    // sentinel text, but wrapped as expected — proving the two files are
    // genuinely independent, not aliases of one another.
    let output_log = state
        .path()
        .join("agents")
        .join("svc")
        .join("logs")
        .join("output.log");
    let attributed = std::fs::read_to_string(&output_log).unwrap();
    assert!(attributed.contains("\"stream\":\"agent-out\""));
    assert!(attributed.contains("KTESIO_USAGE"));

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---- AC-H: the adopted-instance edge case (mirrors interaction.rs's harness, OPPOSITE outcome) ----
//
// Faithfully simulating an ENGINE CRASH so the agent OUTLIVES it (a `kill -9`
// of the engine runs no destructors, so the kill-on-drop handle never fires):
// engine 1's work runs in a SEPARATE child process (a re-exec of THIS test
// binary via the `logs_adoption_helper_subprocess` entry) that starts the
// agent then `std::process::exit`s WITHOUT dropping the engine. The parent
// test then opens a NEW engine over the SAME state dir, which adopts the
// still-live process — and, UNLIKE 4.1's `send_input` (AC-D, a hard
// failure), reading/following it WORKS.

/// Linux AND running under CI (GitHub sets `CI`). Mirrors `adoption.rs`'s /
/// `interaction.rs`'s skip for the heavy re-exec + surviving-orphan harness
/// (#109: an x86 ubuntu-CI-only D-state deadlock, unrelated to this story's
/// logic).
fn is_linux_ci() -> bool {
    OsId::current() == OsId::Linux && std::env::var_os("CI").is_some()
}

/// Whether a pid is still alive. NO OS-cfg here (the gate allowlists only
/// `backends/`); branch on the runtime OS id and shell out, exactly like
/// `adoption.rs`'s/`interaction.rs`'s identical helper.
fn pid_alive(pid: u32) -> bool {
    match OsId::current() {
        OsId::Windows => {
            let out = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output();
            match out {
                Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
                Err(_) => false,
            }
        }
        _ => {
            let exists = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            exists && !proc_pid_is_zombie(pid)
        }
    }
}

/// Whether `/proc/<pid>/stat` reports process state `Z` (zombie). No-ops
/// (returns `false`) off Linux, matching `adoption.rs`'s identical helper.
fn proc_pid_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.split_whitespace().next() == Some("Z"))
        })
        .unwrap_or(false)
}

fn wait_until_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid} still alive)");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Read the pid `fake_agent` announced (`ready pid=<n>`) from its agent.log.
fn wait_for_agent_pid(agent_log: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(agent_log) {
            if let Some(line) = contents.lines().find(|l| l.contains("ready pid=")) {
                if let Some(idx) = line.find("pid=") {
                    if let Ok(pid) = line[idx + 4..].trim().parse::<u32>() {
                        return pid;
                    }
                }
            }
        }
        assert!(Instant::now() < deadline, "agent pid never announced");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run "engine 1" in a SEPARATE child process (mirrors `interaction.rs`'s
/// `run_engine1`): register + start `svc`, then exit WITHOUT dropping the
/// engine (crash semantics). Blocks until the child exits.
fn run_engine1(state: &Path, manifest: &Path) {
    let exe = std::env::current_exe().expect("test exe");
    let status = Command::new(exe)
        .args(["--exact", "logs_adoption_helper_subprocess", "--nocapture"])
        .env("KTESIO_LOGS_ADOPTION_HELPER", "1")
        .env("KTESIO_LOGS_ADOPTION_STATE", state)
        .env("KTESIO_LOGS_ADOPTION_MANIFEST", manifest)
        .status()
        .expect("run engine-1 helper subprocess");
    assert!(
        status.success(),
        "engine-1 helper subprocess failed: {status}"
    );
}

/// The re-exec entry for the "engine 1" work (see the section docs above).
/// When `KTESIO_LOGS_ADOPTION_HELPER` is unset this is a trivial pass (it
/// runs as a normal test in the parent binary too, and does nothing).
#[test]
fn logs_adoption_helper_subprocess() {
    let Ok(_mode) = std::env::var("KTESIO_LOGS_ADOPTION_HELPER") else {
        return; // normal in-process invocation: nothing to do.
    };
    let state = PathBuf::from(std::env::var("KTESIO_LOGS_ADOPTION_STATE").unwrap());
    let manifest = PathBuf::from(std::env::var("KTESIO_LOGS_ADOPTION_MANIFEST").unwrap());

    let engine = Engine::open(Some(state)).expect("engine1 open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest))
        .unwrap();
    facade.start("svc").unwrap();
    // Let a real, substantial pre-crash history accrue (several heartbeats
    // on stdout — this manifest's args only enable --heartbeat-ms, not the
    // stderr counterpart) BEFORE "crashing" — `read_agent_log`/
    // `wait_for_min_lines_per_stream` read the ATTRIBUTED `output.log`,
    // which is populated by THIS engine session's background tailer thread
    // (fix pass, review of #80 — the raw `agent.log`/`agent-stderr.log`
    // files are written directly by the OS, independent of any engine
    // thread, but the ATTRIBUTED view still needs the tailer alive to
    // exist) — so the crash-adoption test below needs GENUINE attributed
    // content already on disk to prove engine 2 can read it.
    let _ = wait_for_min_lines_per_stream(&facade, "svc", 5, 0);
    // Crash: exit WITHOUT dropping the engine, so the kill-on-drop handle
    // never fires and the agent (its own session leader) survives,
    // re-parented to init. This process's OWN background tailer thread
    // dies WITH it (a process exit ends every thread in it, regardless of
    // Drop) — so `output.log` (the ATTRIBUTED capture) never grows again
    // after this point. The RAW `agent.log`/`agent-stderr.log` files are
    // DIFFERENT: `fake_agent` writes them directly via its own OS-level
    // redirect, with zero engine participation (the fix pass's crash-immune
    // guarantee), so THEY keep growing for as long as `fake_agent` itself
    // runs, entirely independent of this process's death — see the main
    // test's doc comment for why this distinction does not change AC-H's
    // OWN claim (which is specifically about the attributed, followable
    // view `read_agent_log`/`read_agent_log_since` expose).
    std::process::exit(0);
}

#[test]
fn adopted_instance_can_be_followed_from_a_fresh_engine_session() {
    // AC-H — the genuinely novel edge case this story surfaces, and the
    // mirror image of 4.1's AC-D test: THERE, `send_input` on an adopted
    // instance FAILS (`InteractionUnavailable` — no recoverable stdin
    // pipe); HERE, by contrast, `read_agent_log`/`read_agent_log_since` on
    // an adopted instance SUCCEEDS — reading only ever needs the
    // deterministically-computed FILE path, not a live handle, so a
    // SECOND engine session (which holds NO capture pipeline of its own for
    // this instance — Task 3's `adopt()` sets `log_capture: None`) can still
    // read everything ENGINE 1 captured before it exited.
    //
    // HONEST SCOPE (verified empirically, not merely assumed): a
    // `std::process::exit` (this harness's crash simulation, matching the
    // REAL crash case — a `SIGKILL` on the engine likewise reclaims every
    // fd the OS holds for it) terminates EVERY thread in that process,
    // including its background output-capture TAILER thread — there is no
    // "the old thread keeps running in the background" outcome; the
    // process is simply gone. `fake_agent` itself SURVIVES (re-parented to
    // init, confirmed via `pid_alive` below) because it is a SEPARATE
    // process in its own group.
    //
    // Fix pass (review of #80) REFINEMENT of this scope, still true to the
    // spirit of the ORIGINAL finding below: `fake_agent`'s raw stdout/
    // stderr are no longer PIPES with "no reader on the other end" — they
    // are DIRECT file redirects (`agent.log`/`agent-stderr.log`), so they
    // keep growing for as long as `fake_agent` itself runs, with ZERO
    // dependency on any engine thread (the crash-immunity this fix pass
    // exists to guarantee). What DOES stop, and stay stopped, is
    // specifically the ATTRIBUTED, followable view (`output.log`) this
    // test's assertions read `read_agent_log`/`read_agent_log_since`
    // through — that view is a derived, best-effort projection the NOW-GONE
    // tailer thread produced, and nothing in a SECOND, merely-adopting
    // session ever resumes producing more of it (by design — Task 3's
    // `adopt()` starts no new tailer either, mirroring `stdin`'s
    // `NoPipe`-on-adoption precedent). So this test proves AC-H's actual,
    // achievable claim about that ATTRIBUTED view specifically: reading an
    // ADOPTED instance's ALREADY-CAPTURED history NEVER ERRORS and NEEDS NO
    // live handle/daemon/subscription — contrasted explicitly with
    // `send_input`'s hard failure on the identical instance (4.1 AC-D) —
    // not that the ATTRIBUTED view magically keeps growing after the sole
    // tailer is gone (a claim this test deliberately does NOT make, unlike
    // an earlier draft that incorrectly assumed it and was caught by this
    // exact test failing against the real system before this fix pass ever
    // existed).
    //
    // Runtime-skip on Windows: this needs the child to genuinely SURVIVE
    // the engine-1 subprocess's exit (Unix re-parenting to init); on
    // Windows `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` kills it when the helper
    // exits, so cross-lifetime survival cannot be simulated there — the
    // SAME reason `adoption.rs`'s/`interaction.rs`'s survivor tests skip
    // Windows. NO `#[cfg]` (data-driven; this file is outside the backends
    // allowlist).
    if OsId::current() == OsId::Windows {
        return;
    }
    // Temporary CI mitigation (#109), mirroring `adoption.rs`/`interaction.rs`:
    // this harness (heavy re-exec + a surviving orphan process) deadlocks
    // uninterruptibly on the x86-64 ubuntu GitHub runner ONLY. Skip it
    // there; #109 tracks the root cause + un-skip.
    if is_linux_ci() {
        return;
    }

    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_logs_manifest(
        manifest.path(),
        "svc",
        &["--heartbeat-ms", "30", "--linger-ms", "600000"],
        "guaranteed",
    );

    // Engine 1 (a subprocess): start `svc`, let a real pre-crash history
    // accrue, then crash (exit without drop).
    run_engine1(state.path(), manifest.path());

    // The AGENT process survives the "crash" (re-parented to init) — but,
    // per the doc comment above, its capture threads (which lived in
    // engine 1's now-gone process) do not.
    let agent_log = agent_log_path(state.path(), "svc");
    let pid = wait_for_agent_pid(&agent_log);
    assert!(pid_alive(pid), "svc must survive the engine crash");

    // Engine 2: open over the SAME state dir -> adopt_orphans re-acquires
    // the still-live process into a handle with NO capture threads of its
    // own (Task 3) — this is the harness proof that AC-H does not secretly
    // depend on engine 2 having spawned anything itself.
    let engine2 = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade2 = engine2.blocking();
    let status = facade2.instance_status("svc").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "a live orphan must be adopted as running"
    );

    // AC-H's core claim: read_agent_log (one-shot) SUCCEEDS from a session
    // that never held a live handle for this instance, returning the FULL
    // pre-crash history engine 1 captured — purely from the
    // deterministically-computed path, no daemon/subscription/live handle
    // needed. Contrast directly with 4.1's
    // `send_input_on_an_adopted_instance_is_interaction_unavailable`: THAT
    // call on this SAME kind of instance returns a hard
    // `InteractionUnavailable` error; THIS one returns real data.
    let (lines, one_shot_cursor) = facade2.read_agent_log("svc").unwrap();
    assert!(
        !lines.is_empty(),
        "the adopted instance's already-captured output must be readable from a fresh session"
    );
    assert!(lines
        .iter()
        .any(|l| l.stream == LogStream::AgentOut && l.text.starts_with("heartbeat ")));
    assert!(
        lines
            .iter()
            .any(|l| l.stream == LogStream::Engine && l.text.contains("-> running")),
        "the start transition's engine line, recorded by engine 1, must also be readable: {lines:?}"
    );

    // read_agent_log_since (the follow primitive) ALSO succeeds — a cursor
    // walk across the retained history returns it in append order, with NO
    // error, exactly like the one-shot read.
    let (since_start, cursor_after_all) = facade2.read_agent_log_since("svc", 0).unwrap();
    assert_eq!(
        since_start.len(),
        lines.len(),
        "read_agent_log_since(0) must agree with read_agent_log's one-shot count \
         (both read the same current-generation content here — no rotation occurred)"
    );
    // M1 (review of #80): read_agent_log's OWN returned cursor must agree
    // with read_agent_log_since's, so a `kt agent logs --follow` invocation
    // can prime its poll loop directly from the one-shot dump's cursor with
    // no separate, discarding priming call.
    assert_eq!(
        one_shot_cursor, cursor_after_all,
        "read_agent_log's returned cursor must match read_agent_log_since(0)'s resulting cursor"
    );
    // A further poll from the end of the retained content honestly reports
    // nothing new (never errors, never fabricates growth) — the accurate,
    // testable expression of "no writer remains" for a session that never
    // itself held a handle.
    let (further, cursor_unchanged) = facade2
        .read_agent_log_since("svc", cursor_after_all)
        .unwrap();
    assert!(further.is_empty());
    assert_eq!(cursor_unchanged, cursor_after_all);

    // Teardown: stop via engine 2 (it holds the re-acquired handle) so no
    // orphan remains.
    facade2.stop("svc", Some(Duration::from_secs(5))).unwrap();
    wait_until_gone(pid, "stop must terminate the adopted process");
}

// ---- Finding A: the crash-kill experiment (empirical proof, fix pass review of #80) ----
//
// Reproduces the adversarial reviewer's exact experiment against the FIXED
// code: a process with the OS's DEFAULT SIGPIPE disposition — deliberately
// NOT `fake_agent`, which (being a Rust binary) installs `SIG_IGN` for
// SIGPIPE at startup like every Rust/Python process, masking exactly the
// failure mode a real, non-Rust/non-Python agent CLI (a shell script, a C
// program, many real-world model-CLI wrappers) would hit — spawned through
// the REAL engine, followed by a simulated engine crash
// (`std::process::exit(0)` WITHOUT dropping the `Engine`, the SAME pattern
// `adoption.rs`'s `run_engine1` already uses to model AD-5 crash scenarios).
//
// BEFORE this fix pass: the still-alive, re-parented, NEVER-EXPLICITLY-
// TOUCHED agent process died within 13-20ms of the engine's crash (the
// reviewer's own measurement), confirmed via exit code 141 (=128+13=
// SIGPIPE) — the engine's crash closed its fd table, vanishing the piped
// stdout's sole read-end reference, so the process's NEXT `write()` got
// `EPIPE` and its default SIGPIPE disposition killed it.
//
// AFTER this fix pass: stdout is a DIRECT file redirect (`Stdio::from`,
// never a pipe — see `SpawnSpec::log_file`'s docs), so the process's
// `write()` never depends on the engine's liveness at all. It must SURVIVE
// and keep writing successfully long after the "crash".

/// `yes` — a real, standard Unix utility (present on Linux and macOS by
/// default), NOT written in Rust or Python: it has the OS's DEFAULT SIGPIPE
/// disposition (unlike `fake_agent`), and prints "y" to stdout forever,
/// completely independent of this test's own logic, until killed or its
/// stdout breaks. Wrapped in `sh -c 'echo $$ > <marker>; exec yes'` SOLELY
/// so the marker file ends up holding the exact PID of the running `yes`
/// process (`exec` replaces the shell's image in place, keeping the SAME
/// pid, and preserves the shell's own SIGPIPE disposition — SIG_DFL,
/// unless something upstream explicitly changed it — across the replace;
/// `std::process::Command` already resets SIGPIPE to SIG_DFL for every
/// spawned child regardless of the ENGINE's own SIG_IGN, which is precisely
/// why this experiment is meaningful to run at all).
fn write_yes_manifest(dir: &Path, pid_marker: &Path) {
    // Single-quote the marker path for the SHELL (handles spaces without
    // needing shell-escapes); this whole shell command then becomes ONE
    // TOML basic string (double-quoted) below — Display (not Debug/`{:?}`,
    // which would inject its OWN literal double quotes and break the TOML)
    // is what keeps the two layers of quoting from colliding.
    let shell_cmd = format!("echo $$ > '{}'; exec yes", pid_marker.display());
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "sigpipe-probe"

[lifecycle.start]
exec = "sh"
args = ["-c", {shell_cmd:?}]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Run "engine 1" in a SEPARATE child process (mirrors `run_engine1`/
/// `adoption.rs`'s identical pattern): register + start `svc` (the `yes`
/// probe), then exit WITHOUT dropping the engine (crash semantics). Blocks
/// until the child exits.
fn run_crash_helper(state: &Path, manifest: &Path) {
    let exe = std::env::current_exe().expect("test exe");
    let status = Command::new(exe)
        .args(["--exact", "crash_kill_helper_subprocess", "--nocapture"])
        .env("KTESIO_CRASH_HELPER", "1")
        .env("KTESIO_CRASH_STATE", state)
        .env("KTESIO_CRASH_MANIFEST", manifest)
        .status()
        .expect("run crash-kill helper subprocess");
    assert!(
        status.success(),
        "crash-kill helper subprocess failed: {status}"
    );
}

/// The re-exec entry for [`run_crash_helper`]. When `KTESIO_CRASH_HELPER` is
/// unset this is a trivial pass (it runs as a normal test in the parent
/// binary too, and does nothing).
#[test]
fn crash_kill_helper_subprocess() {
    let Ok(_mode) = std::env::var("KTESIO_CRASH_HELPER") else {
        return; // normal in-process invocation: nothing to do.
    };
    let state = PathBuf::from(std::env::var("KTESIO_CRASH_STATE").unwrap());
    let manifest = PathBuf::from(std::env::var("KTESIO_CRASH_MANIFEST").unwrap());

    let engine = Engine::open(Some(state)).expect("engine1 open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest))
        .unwrap();
    facade.start("svc").unwrap();
    // Crash IMMEDIATELY after start (no linger): exit WITHOUT dropping the
    // engine, so the kill-on-drop handle never fires. This is the
    // WORST-CASE timing for the pre-fix bug — the engine dies as soon as
    // possible after spawning, maximizing the chance a live pipe's read end
    // would already be gone by the time the child's very next write()
    // happens.
    std::process::exit(0);
}

/// Read the `yes` process's pid directly out of the state DB (the write-
/// ahead spawn record — `agent_runtime.pid`, story 1-6/AD-5), bypassing the
/// crashed engine session entirely (its process no longer exists to ask).
/// Mirrors `usage_row_count`'s identical "read-only direct connection to
/// the same state DB the engine commits to" pattern.
fn read_pid_from_db(state_dir: &Path, name: &str) -> u32 {
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).expect("open state db");
    conn.query_row(
        "SELECT r.pid FROM agent_runtime r \
         JOIN agent_instances i ON i.id = r.instance_id \
         WHERE i.name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .expect("a write-ahead spawn record must exist") as u32
}

#[test]
fn crash_kill_experiment_a_default_sigpipe_process_survives_an_engine_crash() {
    // See the section docs above for the full experiment design/rationale.
    if OsId::current() == OsId::Windows {
        return; // no SIGPIPE / no `yes` on Windows; N/A there (see the fix's own docs).
    }
    if is_linux_ci() {
        return; // #109: same heavy re-exec + surviving-orphan CI mitigation as the other harnesses.
    }

    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let pid_marker = state.path().join("yes.pid");
    write_yes_manifest(manifest.path(), &pid_marker);

    // Engine 1 (a subprocess): start `svc` (the `yes` probe), then crash
    // (exit without drop) as fast as possible after start.
    let crash_at = Instant::now();
    run_crash_helper(state.path(), manifest.path());
    let crashed_after = crash_at.elapsed();

    // Learn the `yes` process's pid — from the DB (the crashed engine
    // session can no longer be asked), cross-checked against the marker
    // file `sh` wrote right before `exec`ing into `yes` (same pid, since
    // `exec` never forks).
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_marker) {
            if let Ok(marker_pid) = contents.trim().parse::<u32>() {
                break marker_pid;
            }
        }
        assert!(Instant::now() < deadline, "yes.pid marker never appeared");
        std::thread::sleep(Duration::from_millis(10));
    };
    let db_pid = read_pid_from_db(state.path(), "svc");
    assert_eq!(
        db_pid, pid,
        "the marker-file pid and the write-ahead record's pid must agree (same process)"
    );

    let agent_log = agent_log_path(state.path(), "svc");
    let len_at_t0 = std::fs::metadata(&agent_log).map(|m| m.len()).unwrap_or(0);
    assert!(
        pid_alive(pid),
        "the yes process must be alive immediately after the simulated crash (pid {pid})"
    );

    // The reviewer's pre-fix measurement: death within 13-20ms of the
    // crash. Wait a window MANY times larger (300ms — ~15-20x) before
    // re-checking, so a pass here is not a lucky race but a genuine,
    // comfortable margin.
    let proof_window = Duration::from_millis(300);
    std::thread::sleep(proof_window);

    let still_alive = pid_alive(pid);
    let len_at_t1 = std::fs::metadata(&agent_log).map(|m| m.len()).unwrap_or(0);

    // Report the exact numbers for the record (visible with --nocapture),
    // mirroring the reviewer's own "confirmed via exit code 141" precision.
    println!(
        "crash-kill experiment: helper subprocess crashed after {crashed_after:?}; \
         yes pid={pid}; agent.log length at crash={len_at_t0} bytes, \
         after a {proof_window:?} wait={len_at_t1} bytes (grew by {} bytes); \
         still alive after the wait={still_alive}",
        len_at_t1.saturating_sub(len_at_t0)
    );

    assert!(
        still_alive,
        "REGRESSION: the yes process (pid {pid}) died within {proof_window:?} of the engine's \
         simulated crash — this is the exact SIGPIPE-on-crash regression this fix pass exists \
         to close (pre-fix, the reviewer measured death within 13-20ms)"
    );
    assert!(
        len_at_t1 > len_at_t0,
        "the yes process must have kept WRITING successfully after the crash, not merely \
         still exist (agent.log length {len_at_t0} -> {len_at_t1})"
    );

    // Teardown: this process was never adopted (no second engine session
    // ran here), so it is a bare orphan — kill it directly.
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    wait_until_gone(
        pid,
        "the yes process must be killable directly after the experiment",
    );
}
