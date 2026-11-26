//! Integration pins for `hekma agent logs --follow`'s RAW-fallback arms on the
//! bounded final drain — the follow loop's after-transition retry window.
//!
//! Coverage slice (census run 42845870146, base main d076569):
//! `crates/hekma/src/cli/agent.rs` missed lines 1919–1923 (the final-drain raw
//! poll: first-show of the un-attributed qualifier, the raw emit + pipe-close
//! return, the cursor advance and the bound-elapsed break) and 2048–2052 (the
//! `note_if_raw_shrunk` body). The STEADY-STATE raw poll and the attributed
//! rotation notices are already pinned in `agent_cli.rs`; these tests pin
//! the same contract observed through the drain's retry loop, where no prior
//! test ever delivered raw content. The drain's OWN qualifier first-show
//! (its `!shown` arm, as opposed to the steady-state poll's identical one)
//! is pinned by the dedicated third test: in the first test the early raw
//! line streams on the first steady-state poll, so the qualifier fires
//! there, not in the drain.
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

/// The late ATTRIBUTED pair — agent-out line and the ENGINE line the
/// final drain exists to catch — appended in ONE `write_all` (round 6 of
/// #404): two separate appends left a drain-poll-lands-between-them window
/// where the drain could see only the first line, settle on the next quiet
/// poll, and spuriously miss the engine line. One write puts both lines in
/// the same poll's read BY CONSTRUCTION, so the test drives DELIVERY of
/// the late attributed pair (the cross-process race the fix closes) and
/// leaves distinguishing the settle rules themselves to the unit pin
/// (`final_drain_settles_only_on_quiet_after_content`).
fn append_captured_late_pair_one_write(
    state_dir: &Path,
    name: &str,
    agent_text: &str,
    engine_text: &str,
) {
    use std::io::Write;
    let path = state_dir
        .join("agents")
        .join(name)
        .join("logs")
        .join("output.log");
    std::fs::create_dir_all(path.parent().unwrap()).expect("create the instance log dir");
    let agent = hekma_engine::LogLine::new(
        name,
        LogStream::AgentOut,
        agent_text,
        "2026-07-20T12:00:00Z",
    );
    let engine =
        hekma_engine::LogLine::new(name, LogStream::Engine, engine_text, "2026-07-20T12:00:01Z");
    let mut bytes = serde_json::to_string(&agent).unwrap();
    bytes.push('\n');
    bytes.push_str(&serde_json::to_string(&engine).unwrap());
    bytes.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open the attributed output log");
    file.write_all(bytes.as_bytes())
        .expect("append the late attributed pair");
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
    spawn_follow_with_env(ctx, state_dir, name, &[])
}

/// Spawn the follow child with EXTRA environment pairs — the shape tests
/// that enable the test-only drain-entry marker
/// (`HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER`) synchronize on.
fn spawn_follow_with_env(
    ctx: &TestContext,
    state_dir: &Path,
    name: &str,
    extra_env: &[(&str, &str)],
) -> std::process::Child {
    let out_path = ctx.project_dir.join("follow-stdout.ndjson");
    let err_path = ctx.project_dir.join("follow-stderr.txt");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hekma"));
    command
        .args(["agent", "logs", name, "--follow", "--json"])
        .current_dir(&ctx.project_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .stdout(std::fs::File::create(&out_path).expect("create follow stdout file"))
        .stderr(std::fs::File::create(&err_path).expect("create follow stderr file"));
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command
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

/// Poll a follow-child's redirected stderr file until it contains `marker` —
/// the same observed-output synchronization as `wait_stdout_contains`, for
/// notices and test markers that ride stderr (stdout stays pure NDJSON).
fn wait_stderr_contains(ctx: &TestContext, marker: &str) {
    let err_path = ctx.project_dir.join("follow-stderr.txt");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let seen = std::fs::read_to_string(&err_path).unwrap_or_default();
        if seen.contains(marker) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "follow never emitted `{marker}` on stderr; stderr so far={seen}"
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
    // backlog line the parent waits to see committed, and an `agent.log`
    // seeded with ONE early raw line BEFORE the spawn. The backlog prime
    // re-opens the window for that early line, so it can only stream on the
    // loop's FIRST steady-state poll; the parent WAITS for it on stdout
    // before touching the file again. Seeing it proves the first poll (raw
    // block included) has fully run, and the status check in the same
    // iteration sees the never-running instance and enters the drain. The
    // late write that follows — a growth-only APPEND of the same file, so
    // the shrink notice stays out of the picture BY CONSTRUCTION (an append
    // never shortens the file; a truncate-and-rewrite would briefly report
    // length 0 and a drain poll in that window would misread it as a
    // shrink) — is causally after every steady-state read: on any runner
    // speed the drain's retrying raw poll (2s bound, 20ms retries) is the
    // ONLY reader left to deliver it. No fixed sleep to race: the
    // synchronization is the observed early line.
    let (ctx, state) = registered_mock("rawl");
    let state_dir = state.project_dir.as_path();
    append_captured_line(state_dir, "rawl", "backlog line");

    let raw_path = raw_agent_log_path(state_dir, "rawl");
    std::fs::create_dir_all(raw_path.parent().unwrap()).expect("create the raw log dir");
    std::fs::write(&raw_path, "early raw line\n").expect("write the early raw line");

    let mut child = spawn_follow(&ctx, state_dir, "rawl");
    // The one-shot dump carries the attributed backlog…
    wait_stdout_contains(&ctx, "backlog line");
    // …and the first steady-state poll streams the seeded early line: only
    // once it is observed is the drain provably the sole remaining reader.
    wait_stdout_contains(&ctx, "early raw line");

    // The late raw content, landing while the drain retries on empty polls.
    // APPEND, never rewrite: `fs::write` opens with O_TRUNC, so a
    // truncate-and-rewrite briefly reports length 0 and a drain poll landing
    // in that window would see a shrink and stream the early line twice. The
    // append keeps the length monotonically growing — the growth the comment
    // above promises holds by construction, not by timing luck.
    {
        use std::io::Write as _;
        let mut raw_log = std::fs::OpenOptions::new()
            .append(true)
            .open(&raw_path)
            .expect("open the raw log for the growth-only append");
        raw_log
            .write_all(b"late raw line\n")
            .expect("append the late raw line");
    }

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
        vec!["backlog line", "early raw line", "late raw line"],
        "the late raw line must be emitted, as NDJSON, through the drain — after the \
         one-shot backlog and the first poll's early line"
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
fn logs_follow_final_drain_owns_the_qualifier_first_show() {
    // The qualifier's once-per-session first-show has TWO call sites — the
    // steady-state raw poll's and the drain's identical twin. In the test
    // above the early raw line streams on the first steady-state poll, so
    // the qualifier there fires on the STEADY-STATE arm and the drain's own
    // `!shown` arm stays dark. Pin it in isolation: the child is spawned
    // under HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER, so the follow ITSELF
    // announces drain entry with a stderr marker printed between the status
    // check that ended the steady-state loop and the drain's first poll.
    // The parent creates `agent.log` (the session's first raw content) only
    // AFTER observing that marker, so the file provably did not exist
    // during ANY steady-state read: the session's first non-empty raw read
    // happens inside the drain's retry loop, firing the drain's own
    // qualifier first-show and streaming the line as NDJSON; the drain
    // then settles on the first QUIET poll after that content, not on
    // the content itself (the settle-rule distinction is pinned by
    // `final_drain_settles_only_on_quiet_after_content`). No poll-interval
    // margin is involved anywhere
    // — the previous drive raced its attributed marker against the 300 ms
    // interval (a parent descheduled past it delivered the marker through
    // the drain instead and failed spuriously); the only bound left now is
    // the drain's own 2 s retry window vs the parent's marker-to-create
    // latency, a single observed file create.
    let (ctx, state) = registered_mock("rawq");
    let state_dir = state.project_dir.as_path();
    append_captured_line(state_dir, "rawq", "backlog line");

    let mut child = spawn_follow_with_env(
        &ctx,
        state_dir,
        "rawq",
        &[("HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER", "1")],
    );
    // The one-shot dump carries the attributed backlog…
    wait_stdout_contains(&ctx, "backlog line");
    // …and the follow itself announces drain entry on stderr: only once
    // that marker is observed is the raw file created, strictly after the
    // last steady-state read.
    wait_stderr_contains(&ctx, "logs-follow: final drain entered");

    // The session's first raw content, created when only drain polls remain.
    let raw_path = raw_agent_log_path(state_dir, "rawq");
    std::fs::create_dir_all(raw_path.parent().unwrap()).expect("create the raw log dir");
    std::fs::write(&raw_path, "drain raw line\n").expect("create the raw log with its only line");

    let status = wait_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(0),
        "follow must exit 0; stderr={}",
        follow_stderr(&ctx)
    );

    let stdout = follow_stdout(&ctx);
    let texts = assert_ndjson_texts(&stdout, "rawq");
    assert_eq!(
        texts,
        vec!["backlog line", "drain raw line"],
        "the drain's raw line streams after the dump — the observed drain-entry \
         marker is the proof no steady-state read ever saw the raw file"
    );

    let stderr = follow_stderr(&ctx);
    let qualifier = "may be un-attributed raw passthrough from agent.log";
    assert_eq!(
        stderr.matches(qualifier).count(),
        1,
        "the qualifier is surfaced ONCE — its first display of the session, \
         on the drain's own arm; stderr={stderr}"
    );
    let qualifier_at = stderr
        .find(qualifier)
        .expect("the qualifier must ride stderr");
    let exit_note_at = stderr
        .find("no further output")
        .expect("the follow-exit note must ride stderr");
    assert!(
        qualifier_at < exit_note_at,
        "the qualifier fires inside the drain, before the exit note that \
         ends it; stderr={stderr}"
    );
}

#[test]
fn logs_follow_final_drain_catches_a_late_attributed_engine_line() {
    // Round 5 of #404: every drain test above stages its late content on
    // the RAW agent.log half, but the race the PR is justified by is the
    // ATTRIBUTED one — `transition_with_log_capture` persists the new
    // state FIRST and writes output.log's engine line afterwards, so
    // `logs --follow` (a separate process) can enter the final drain
    // before that line exists. Drive the attributed half directly,
    // synchronized on the drain-entry marker like the tests above: an
    // agent-out line AND the engine line land in output.log STRICTLY
    // after every steady-state read — in ONE write (round 6 of #404), so
    // a single drain poll observes both together and no poll can land
    // between them. The drain must deliver BOTH, in file order.
    // Distinguishing the settle rules themselves — first-content vs
    // quiet-after-content — is NOT this test's job: one atomic write
    // passes under either rule, so that distinction is unit-pinned in
    // `agent.rs` (`final_drain_settles_only_on_quiet_after_content`)
    // rather than claimed here.
    let (ctx, state) = registered_mock("rawe");
    let state_dir = state.project_dir.as_path();
    append_captured_line(state_dir, "rawe", "backlog line");

    let mut child = spawn_follow_with_env(
        &ctx,
        state_dir,
        "rawe",
        &[("HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER", "1")],
    );
    // The one-shot dump carries the attributed backlog…
    wait_stdout_contains(&ctx, "backlog line");
    // …then the follow announces drain entry; only after that marker —
    // provably inside the drain, past every steady-state read — does the
    // late ATTRIBUTED pair land in output.log as one write: agent output
    // first, then the engine line the drain exists to catch.
    wait_stderr_contains(&ctx, "logs-follow: final drain entered");
    append_captured_late_pair_one_write(state_dir, "rawe", "late agent line", "late engine line");

    let status = wait_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(0),
        "follow must exit 0; stderr={}",
        follow_stderr(&ctx)
    );

    let stdout = follow_stdout(&ctx);
    let texts = assert_ndjson_texts(&stdout, "rawe");
    assert_eq!(
        texts,
        vec!["backlog line", "late agent line", "late engine line"],
        "the drain must deliver the late ATTRIBUTED engine line — the exact \
         cross-process write it exists to catch — after the agent-out line, \
         in file order"
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
    // complete-line end). The parent then waits for the follow's OWN
    // drain-entry marker (the test-only HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER
    // knob, the same synchronization the qualifier test above uses) before
    // replacing the file — atomically, via rename, the same
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

    let mut child = spawn_follow_with_env(
        &ctx,
        state_dir,
        "rawsh",
        &[("HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER", "1")],
    );
    // Wait until the raw line itself is committed — poll 1's raw read has
    // set the cursor at the complete-line end — and then for the follow's
    // OWN drain-entry marker: under HEKMA_TEST_LOGS_FOLLOW_DRAIN_MARKER the
    // status check that ends the steady-state loop prints one stderr marker
    // before the drain's first poll, so the rename below provably lands
    // mid-drain. No poll-interval margin is involved anywhere — the
    // previous fixed 500 ms settle raced the replace against the 300 ms
    // interval.
    wait_stdout_contains(&ctx, long_line);
    wait_stderr_contains(&ctx, "logs-follow: final drain entered");

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
