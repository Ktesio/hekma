//! Integration pins for `hekma agent logs --follow`'s RAW-fallback arms on the
//! bounded final drain — the follow loop's after-transition retry window.
//!
//! Coverage slice (census run 42845870146, base main d076569):
//! `crates/hekma/src/cli/agent.rs` missed lines 1919–1923 (the final-drain raw
//! poll: first-show of the un-attributed qualifier, the raw emit + pipe-close
//! return, the cursor advance and the bound-elapsed break) and 2048–2052 (the
//! `note_if_raw_shrunk` body). The STEADY-STATE raw poll and the attributed
//! rotation notices are already pinned in `agent_cli.rs`; these two tests pin
//! the same contract observed through the drain's retry loop, where no prior
//! test ever delivered raw content.
//!
//! Cross-OS by construction (the `agent_cli.rs` rotation-test spine): a
//! `registered` (never-started) instance is not `running` on every OS, so the
//! follow loop reaches the final-drain exit path everywhere — no live child,
//! no Unix-only survival harness. The harness helpers here are the same
//! shapes `agent_cli.rs` uses privately (`registered_mock`, the attributed
//! append, the NDJSON wire assert); they are re-declared locally so this
//! slice stays conflict-free of the open review queue's files.

mod helpers;

use std::path::Path;
use std::time::Duration;

use hekma_engine::LogStream;
use helpers::{run_hekma_agent, TestContext};

/// The RAW, legacy `agent.log` the raw fallback polls —
/// `<state>/agents/<name>/logs/agent.log`, next to the attributed
/// `logs/output.log`. Unlike the attributed capture this file is NEVER
/// rotated: a length below the follow cursor is a truncation or a recreate,
/// which is exactly what [`note_if_raw_shrunk`] names.
fn raw_agent_log_path(state_dir: &Path, name: &str) -> std::path::PathBuf {
    state_dir
        .join("agents")
        .join(name)
        .join("logs")
        .join("agent.log")
}

/// The ATTRIBUTED output log (one serialized `LogLine` per JSON line) — the
/// same bytes the engine's capture thread appends, so the CLI reads them back
/// through the ordinary `read_agent_log_since` path with nothing stubbed.
fn append_captured_line(state_dir: &Path, name: &str, text: &str) {
    use std::io::Write;
    let path = state_dir
        .join("agents")
        .join(name)
        .join("logs")
        .join("output.log");
    std::fs::create_dir_all(path.parent().unwrap()).expect("create the instance log dir");
    let line = hekma_engine::LogLine::new(name, LogStream::AgentOut, text, "2026-07-20T12:00:00Z");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open the attributed output log");
    writeln!(file, "{}", serde_json::to_string(&line).unwrap()).expect("append a log line");
    file.flush().expect("flush the attributed output log");
}

/// Assert every non-empty stdout line is ONE complete, compact `LogLine` JSON
/// object with the frozen key-set at the pinned schema version, and return the
/// `text` values in order (the same wire contract `agent_cli.rs` asserts —
/// stdout is pure NDJSON, every notice rides stderr).
fn assert_ndjson_texts(stdout: &str, instance: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for (index, raw) in stdout.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let line: serde_json::Value = serde_json::from_str(raw).unwrap_or_else(|e| {
            panic!("NDJSON line {index} is not valid JSON: {e}\n{raw}\nfull stdout:\n{stdout}")
        });
        let mut keys: Vec<&str> = line
            .as_object()
            .expect("each stdout line is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["at", "instance", "schema_version", "stream", "text"],
            "the LogLine key-set is frozen; line {index}: {line}"
        );
        assert_eq!(line["schema_version"], serde_json::json!(1), "{line}");
        assert_eq!(line["instance"], serde_json::json!(instance), "{line}");
        let stream = line["stream"].as_str().expect("stream is a string");
        assert!(
            matches!(stream, "agent-out" | "agent-err" | "engine"),
            "unexpected stream token `{stream}`",
        );
        texts.push(line["text"].as_str().expect("text is a string").to_string());
    }
    assert!(
        !stdout.contains("[agent-out]")
            && !stdout.contains("[agent-err]")
            && !stdout.contains("[engine]"),
        "stdout must be pure NDJSON, not the human form; stdout={stdout}",
    );
    texts
}

/// Register a `mock` instance (never-started — not `running` on any OS).
fn registered_mock(name: &str) -> (TestContext, TestContext) {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let run = run_hekma_agent(
        &["agent", "register", name, "--kind", "mock"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert!(
        run.success,
        "register should succeed; stderr={}",
        run.stderr
    );
    (ctx, state)
}

/// Spawn `hekma agent logs <name> --follow --json` with stdout/stderr
/// redirected to files the parent can poll while the child runs.
fn spawn_follow(ctx: &TestContext, state_dir: &Path, name: &str) -> std::process::Child {
    let out_path = ctx.project_dir.join("follow-stdout.ndjson");
    let err_path = ctx.project_dir.join("follow-stderr.txt");
    std::process::Command::new(env!("CARGO_BIN_EXE_hekma"))
        .args(["agent", "logs", name, "--follow", "--json"])
        .current_dir(&ctx.project_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .stdout(std::fs::File::create(&out_path).expect("create follow stdout file"))
        .stderr(std::fs::File::create(&err_path).expect("create follow stderr file"))
        .spawn()
        .expect("spawn hekma agent logs --follow --json")
}

/// Poll a follow-child's redirected stdout file until it contains `marker`,
/// then hand the child back for the on-disk drive + exit wait.
fn wait_stdout_contains(ctx: &TestContext, marker: &str) {
    let out_path = ctx.project_dir.join("follow-stdout.ndjson");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let seen = std::fs::read_to_string(&out_path).unwrap_or_default();
        if seen.contains(marker) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "follow never emitted `{marker}`; stdout so far={seen}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait for the follow child to exit on its own — it must never hang.
fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let exit_deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(
            std::time::Instant::now() < exit_deadline,
            "hekma agent logs --follow must not hang"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn follow_stdout(ctx: &TestContext) -> String {
    std::fs::read_to_string(ctx.project_dir.join("follow-stdout.ndjson"))
        .expect("read follow stdout")
}

fn follow_stderr(ctx: &TestContext) -> String {
    std::fs::read_to_string(ctx.project_dir.join("follow-stderr.txt")).expect("read follow stderr")
}

#[test]
fn logs_follow_final_drain_streams_late_raw_lines_with_the_qualifier_once() {
    // The follow loop's raw fallback polls `agent.log` in TWO places: the
    // steady-state loop (pinned already) and the BOUNDED FINAL DRAIN that
    // runs after the instance left `running` — the drain RETRIES while a
    // poll comes back empty (its documented purpose: a writer that has not
    // caught up yet must not lose its last lines). No prior test ever
    // delivered raw content THROUGH the drain, so its raw block was dead in
    // the census: the first-show of the un-attributed qualifier, the raw
    // NDJSON emit, and the cursor advance all only existed on the
    // steady-state path.
    //
    // Drive: a `registered` (never-started) instance — not `running`, so the
    // loop enters the drain after its first poll — with an attributed
    // backlog line the parent waits to see committed. The raw `agent.log` is
    // created only after a settle that clears the first steady-state poll
    // (the one-shot read ~50ms + the 300ms poll interval): the write lands
    // inside the drain's retry window (2s bound, 20ms retries), and the
    // drain's raw poll must deliver the line as NDJSON plus the qualifier
    // notice exactly once. The 500ms parent settle puts the write past poll
    // 1 by a wide margin, so the line can only surface through the drain —
    // the same file-poll + on-disk drive method the rotation test in
    // `agent_cli.rs` established.
    let (ctx, state) = registered_mock("rawl");
    let state_dir = state.project_dir.as_path();
    append_captured_line(state_dir, "rawl", "backlog line");

    let mut child = spawn_follow(&ctx, state_dir, "rawl");
    wait_stdout_contains(&ctx, "backlog line");
    std::thread::sleep(Duration::from_millis(500));

    // The late raw content, landing while the drain retries on empty polls.
    let raw_path = raw_agent_log_path(state_dir, "rawl");
    std::fs::create_dir_all(raw_path.parent().unwrap()).expect("create the raw log dir");
    std::fs::write(&raw_path, "late raw line\n").expect("write the late raw line");

    let status = wait_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(0),
        "follow must exit 0; stderr={}",
        follow_stderr(&ctx)
    );

    let stdout = follow_stdout(&ctx);
    let texts = assert_ndjson_texts(&stdout, "rawl");
    assert_eq!(
        texts,
        vec!["backlog line", "late raw line"],
        "the late raw line must be emitted, as NDJSON, after the attributed backlog"
    );

    let stderr = follow_stderr(&ctx);
    let qualifier = "may be un-attributed raw passthrough from agent.log";
    assert!(
        stderr.contains(qualifier),
        "the un-attributed qualifier must ride stderr once the raw line appears; stderr={stderr}"
    );
    assert_eq!(
        stderr.matches(qualifier).count(),
        1,
        "the qualifier is surfaced ONCE PER SESSION, never per poll; stderr={stderr}"
    );
    assert!(
        stderr.contains("no further output"),
        "the honest follow-exit note belongs on stderr; stderr={stderr}"
    );
}

#[test]
fn logs_follow_reports_a_shrunk_raw_log_as_truncated_not_rotated() {
    // `agent.log` is NEVER rotated, so a raw length below the follow cursor
    // is a truncation (or a recreate) — and `note_if_raw_shrunk` gives it
    // its OWN honest notice: the attributed-rotation remediation would lie
    // here (the one-shot command never re-reads the raw file, so whatever
    // the truncation ate is gone from every surface). The notice body was
    // dead in the census — no test ever shrank the raw file mid-follow —
    // and the drain's call site is the one this drive reaches naturally.
    //
    // Drive: the raw file exists at follow start with one long line, which
    // the steady-state poll streams (qualifier fired there, cursor at the
    // complete-line end). Once the parent SEES that line committed, the
    // follow has moved into the drain (retrying on empty every 20ms); the
    // parent then replaces the file — atomically, via rename, the same
    // never-torn shape `rotate_generations` itself uses — with a SHORTER
    // one. The drain's next raw poll observes cursor > length, snaps to the
    // new end, delivers nothing (the engine's documented shrink semantics),
    // and must print the shrink notice on stderr.
    let (ctx, state) = registered_mock("rawsh");
    let state_dir = state.project_dir.as_path();
    append_captured_line(state_dir, "rawsh", "backlog line");

    let raw_path = raw_agent_log_path(state_dir, "rawsh");
    std::fs::create_dir_all(raw_path.parent().unwrap()).expect("create the raw log dir");
    let long_line = "a long raw line that the truncation is going to eat";
    std::fs::write(&raw_path, format!("{long_line}\n")).expect("write the long raw line");

    let mut child = spawn_follow(&ctx, state_dir, "rawsh");
    // Wait until the raw line itself is committed — by then poll 1's raw
    // read has set the cursor at the complete-line end and the follow has
    // entered the drain's retry window (poll 1 runs ~350ms in; the 500ms
    // settle clears it by a wide margin, so the replace lands mid-drain).
    wait_stdout_contains(&ctx, long_line);
    std::thread::sleep(Duration::from_millis(500));

    // The recreate: stage a SHORTER fresh file next to it and rename it over
    // (atomic — a concurrent reader sees the old or the new generation,
    // never a torn prefix; the length drop below the cursor is the shape
    // `note_if_raw_shrunk` exists to name).
    let shorter = raw_path.with_extension("log.shorter.tmp");
    std::fs::write(&shorter, "short\n").expect("write the shorter replacement");
    std::fs::rename(&shorter, &raw_path).expect("rename the shorter raw log into place");

    let status = wait_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(0),
        "a shrunk raw log is a NOTICE, never an error; stderr={}",
        follow_stderr(&ctx)
    );

    let stdout = follow_stdout(&ctx);
    let texts = assert_ndjson_texts(&stdout, "rawsh");
    assert_eq!(
        texts,
        vec!["backlog line", long_line],
        "the drain delivers NOTHING for a shrunk raw file (the engine snaps to the \
         new end); the pre-truncation lines stay in append order"
    );

    let stderr = follow_stderr(&ctx);
    assert!(
        stderr.contains("shrank (truncated or recreated)"),
        "the raw shrink must be named as a truncation/recreate, not a rotation; stderr={stderr}"
    );
    assert!(
        stderr.contains("does not re-read it"),
        "the notice must state the honest loss — the one-shot logs command never \
         re-reads the raw file; stderr={stderr}"
    );
    let qualifier = "may be un-attributed raw passthrough from agent.log";
    assert_eq!(
        stderr.matches(qualifier).count(),
        1,
        "the qualifier fired once at poll 1's raw stream and must not repeat; stderr={stderr}"
    );
}
