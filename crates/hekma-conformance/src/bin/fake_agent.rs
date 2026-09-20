//! `fake_agent` — a tiny, cross-platform, spawnable test agent (story 1.4, AD-3).
//!
//! A DEV/TEST artifact: the engine's start/stop integration tests point a
//! manifest adapter's `[lifecycle.start]` `exec` at this binary so the supervisor
//! spawns a REAL process to launch, supervise, and kill — proving supervision end
//! to end (1-3's `ScriptedFakeAgent` was inert). It is pure `std` with NO
//! OS-conditional code (the OS-cfg CI gate applies to `hekma-conformance` too),
//! so it builds and runs identically on Linux, macOS, and Windows.
//!
//! ## Behaviors (selected by args)
//!
//! * (default) print a ready line to stdout, then loop-sleep "forever" (until the
//!   supervisor kills it) — proves `start → running` and `stop → killed`.
//! * `--exit-fast <code>`  exit immediately with `<code>` — proves AC2
//!   launch-failure / immediate non-zero exit during startup.
//! * `--spawn-child`  before looping, spawn ANOTHER copy of this binary (also
//!   looping) as a child — proves AC3 "no process of the instance survives"
//!   catches the WHOLE process group / Job, not just the parent PID. The child's
//!   pid is printed as `child-pid=<n>` so a test can assert the child is gone too.
//! * `--linger-ms <ms>`  after receiving no signal, keep running for at least
//!   `<ms>` before self-exiting. Tests set the stop window SHORTER than this so
//!   the graceful window elapses and the supervisor's forced kill is exercised —
//!   the cfg-free way to force escalation (a SIGTERM-ignoring handler would need
//!   OS-cfg, which the gate forbids here). With no `--linger-ms` the process
//!   loops effectively forever (a very long sleep), so a normal graceful stop
//!   still ends it promptly via the kill.
//! * `--heartbeat-ms <ms>`  print an incrementing `heartbeat <n>` line to stdout
//!   every `<ms>` and flush (story 1-5). When the process is SIGSTOP'd the whole
//!   process freezes, so its captured log STOPS growing; SIGCONT resumes it — the
//!   OBSERVABLE suspension proof for the guaranteed pause path. With no
//!   `--heartbeat-ms` the loop is a quiet sleep (existing 1-4 tests that only
//!   assert `ready`/lifecycle are unaffected). Pure `std`, NO OS-cfg.
//! * `--heartbeat-stderr-ms <ms>` (story 4-2)  print an incrementing
//!   `stderr-heartbeat <n>` line to STDERR every `<ms>` and flush — the STDERR
//!   counterpart of `--heartbeat-ms`, independent of it (its own counter, own
//!   interval) so a test can combine BOTH flags to produce known, deterministic,
//!   simultaneously-attributable content on stdout AND stderr without racing —
//!   the vehicle the log-capture-attribution tests (`agent-out`/`agent-err`)
//!   need. With no `--heartbeat-stderr-ms` nothing is written to stderr by this
//!   mechanism (existing tests unaffected). Pure `std`, NO OS-cfg.
//! * `--crash-after-ms <ms>` (+ optional `--crash-with <code>`)  run normally
//!   (announcing readiness, heartbeating if asked) for `<ms>`, THEN exit with
//!   `<code>` (default 1) — simulating an UNREQUESTED crash AFTER the readiness
//!   window (story 1-6). Distinct from `--exit-fast`, which exits DURING startup
//!   (a launch failure); `--crash-after-ms` stays alive long enough to reach
//!   `running`, so the supervisor's reaper detects the later `Exited` as a crash
//!   and the Restart Policy fires. The `--heartbeat-ms` line count proves a
//!   RESTARTED instance is alive again. Pure `std`, NO OS-cfg.
//! * `--crash-times <N>` (+ `--crash-state <path>`)  BOUND the self-crash to the
//!   first `<N>` LAUNCHES of the instance, so a restarted process eventually STOPS
//!   crashing and stays up. Because each restart is a FRESH process (an in-memory
//!   counter cannot survive it), the count is kept in a tiny `--crash-state` file
//!   the engine's Agent Home persists across restarts: on each launch the process
//!   reads the integer there (default 0); while it is `< N` it increments+persists
//!   and crashes (after `--crash-after-ms`), and once it reaches `N` it does NOT
//!   crash — it lingers normally. This makes an `on-failure` restart test
//!   DETERMINISTIC: with `--crash-times 1` the instance crashes exactly once, is
//!   restarted once, then stays `running` (its restart count is stably 1 and a
//!   clean stop cannot race a second crash). ABSENT (or without `--crash-state`),
//!   the self-crash is UNBOUNDED (every launch crashes) — the existing crash-loop
//!   behavior is unchanged. Pure `std`, NO OS-cfg (a plain file read/write).
//!
//! The binary writes a small marker file (`--marker <path>`) on startup if asked,
//! so a test can confirm it actually ran without racing on stdout capture.
//!
//! * `--dump <path>` (story 2-2)  write a small observation file at startup: the
//!   full received argv (one `arg=<token>` line each) followed by every
//!   environment variable (one `env=<KEY>=<VALUE>` line each). The engine's
//!   start-seam config mapping proof reads this back to confirm a mapped unified
//!   key landed in the agent's native mechanism — a FLAG in the args, or an ENV
//!   var in the environment — WITHOUT racing on stdout capture. Pure `std`, NO
//!   OS-cfg. (A FILE-target mapping is observed directly as a rendered file in the
//!   Agent Home, so it needs no dump.)
//! * `--emit-usage <N>` (story 3-1)  after announcing readiness, emit `<N>`
//!   self-reported usage sentinel lines — `KTESIO_USAGE {json}` — on stdout, with
//!   monotonic `sequence` 0..N and FIXED token sentinels (`input_tokens = 10`,
//!   `output_tokens = 20` per event), spaced across the loop so the engine's reaper
//!   ingests them into the Usage Ledger. This is the `self-reported` half of FR-19:
//!   the engine parses these captured lines into UsageEvents. Determinism: the test
//!   waits for the KNOWN number of committed rows (the DB is the source of truth),
//!   NOT a wall-clock sleep. Pure `std`, NO OS-cfg (the parser + this emitter share
//!   the documented convention; the OS-cfg gate covers this crate).
//! * `--replay-usage` (story 3-1)  after the `--emit-usage` batch, re-emit
//!   `sequence 0` ONCE (a DELAYED/replayed batch). The engine's ledger dedup must
//!   recognize it and NOT double-count — the AC-A no-double-count proof.
//! * `--usage-input-tokens <N>` / `--usage-output-tokens <N>` (story 3-1)  override
//!   the FIXED per-event token sentinels so a test can emit an arbitrary value —
//!   notably `u64::MAX` — to prove the storage boundary SATURATE-CLAMPS the `u64`
//!   into SQLite's signed `i64` column (a positive `i64::MAX`) rather than a raw
//!   `as i64` that bit-wraps NEGATIVE and poisons the billing SUM (the C1/C2
//!   boundary). Default to the fixed sentinels when absent.
//! * `--usage-cached-tokens <N>` (story 14-6)  stamp each emitted usage line with
//!   an OPTIONAL `cached_tokens` field — the cached SUBSET of `input_tokens` under
//!   the engine's INPUT-INCLUSIVE invariant. `0` (the default) omits the field, so
//!   the emitted line stays byte-identical to the pre-14-6 convention. A test can
//!   thus drive a cached-bearing event through the self-reported ingest path
//!   end-to-end (ledger column + cost re-split + budget counting).
//! * `--final-usage-no-newline` (story 3-1)  after the batch (+ any replay), emit
//!   ONE more usage line WITHOUT a trailing newline (`sequence = emit_usage`), then
//!   exit immediately. The process dies with a half-line in the log, so ONLY the
//!   engine's TERMINAL drain-on-reap can rescue it — the H1 under-count proof (a
//!   mid-run drain, which stops at the last newline, would strand it forever).
//! * `--echo-stdin` (story 4-1)  spawn a dedicated thread that reads stdin
//!   line-by-line and echoes each as `stdin: <line>` to stdout (flushed),
//!   captured into `agent.log` exactly like `heartbeat <n>` lines. Runs
//!   independent of the main loop — coexists with `--heartbeat-ms` /
//!   `--linger-ms`. This is the deterministic observation vehicle for
//!   `send_input`: a test writes a known line via the engine's stdin pipe,
//!   then polls the captured log for `stdin: <line>` (never a wall-clock
//!   sleep). Pure `std`, NO OS-cfg.
//! * `--observed-calls <N>` (story 3-4)  ENGINE-OBSERVED mode: after announcing
//!   readiness, make `<N>` OpenAI-compatible completion requests to the `base_url`
//!   the engine INJECTED into this process's environment (`OPENAI_BASE_URL` — the
//!   env var an `engine-observed` manifest maps `metering.base_url` onto). Each is a
//!   minimal `POST <base_url>/v1/chat/completions`; the engine's loopback forward
//!   listener relays it to a test upstream stub and skims the `usage` out of the
//!   response into the ledger. This is the `engine-observed` half of FR-19: the
//!   agent reports NOTHING itself — the engine OBSERVES its model traffic. A tiny
//!   pure-`std` HTTP/1.1 client (a raw `TcpStream` — NO dependency, NO OS-cfg) makes
//!   the calls, count-bounded so a test waits for `<N>` committed observed rows (the
//!   DB is the source of truth), never a wall-clock sleep. Pure `std`, NO OS-cfg.
//! * `--observed-stream-calls <N>` (story 12-2)  the STREAMING sibling of
//!   `--observed-calls`: each POST body sets `"stream": true` (no
//!   `stream_options` of its own — the engine's listener injects
//!   `include_usage`), and the call DISCARDS the SSE response like any real
//!   streaming client would. The upstream stub (and the engine) sees a
//!   streaming request whose usage arrives only in the terminal SSE frame —
//!   the end-to-end vehicle proving streamed completions are metered. Pure
//!   `std`, NO OS-cfg.
//! * `--sniff-stdin-at-startup` (story 4-1 fix pass, HIGH finding, review of
//!   #79)  BEFORE announcing readiness (before the ready line, before
//!   `--marker`, before anything else), synchronously BLOCK reading ONE line
//!   from stdin (`io::stdin().read_line(..)`). This mimics a common real-CLI
//!   "sniff for piped input at startup" idiom — NOT the `--echo-stdin`
//!   background-thread mechanism (which runs independent of startup and
//!   never blocks it). Under `Stdio::null()` (an adapter that declares no
//!   interaction support) this returns immediately (EOF) and startup
//!   proceeds normally; under an unconditionally-piped stdin whose write end
//!   nothing ever closes, this call would hang forever — the regression this
//!   flag exists to prove does NOT happen once piping is gated on the
//!   declared Capability. Pure `std`, NO OS-cfg.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

// This whole binary runs only as a SPAWNED SUBPROCESS of the supervision tests,
// so a coverage harness (tarpaulin) that instruments the PARENT process can never
// record its lines — they show as 0% and would drag the workspace number down
// dishonestly. Exclude the bin from coverage (the same `#[cfg(not(tarpaulin_include))]`
// convention `kt`'s `main`/`run_cli` use). Its BEHAVIOR is proven by the
// supervision tests that spawn and kill it, not by line coverage.

/// Parsed invocation options.
struct Opts {
    exit_fast: Option<i32>,
    spawn_child: bool,
    linger: Duration,
    marker: Option<PathBuf>,
    /// Dump the received argv + environment to this file at startup (story 2-2:
    /// the config-mapping observation point). `None` = no dump.
    dump: Option<PathBuf>,
    /// Heartbeat interval (story 1-5). `None` = no heartbeat (quiet sleep loop).
    heartbeat: Option<Duration>,
    /// STDERR heartbeat interval (story 4-2) — independent of `heartbeat`
    /// (its own counter, own interval). `None` = nothing written to stderr.
    heartbeat_stderr: Option<Duration>,
    /// Crash AFTER this interval (story 1-6): run normally, then exit non-zero.
    /// `None` = no self-crash (the linger loop governs exit).
    crash_after: Option<Duration>,
    /// The exit code the `--crash-after-ms` self-crash uses (default 1).
    crash_with: i32,
    /// BOUND the self-crash to the first `<N>` launches (see the module docs).
    /// `None` = unbounded (every launch crashes — the existing behavior). Only has
    /// an effect together with `--crash-state` (the cross-restart counter file).
    crash_times: Option<u64>,
    /// The tiny file that carries the launch-crash counter ACROSS restarts (each
    /// restart is a fresh process), so `--crash-times` can stop crashing after the
    /// bound. `None` = no persisted counter (the bound cannot be enforced).
    crash_state: Option<PathBuf>,
    /// Emit this many self-reported usage sentinel lines (story 3-1). `0` = none.
    emit_usage: u64,
    /// After the usage batch, re-emit `sequence 0` once — a replayed batch for the
    /// no-double-count proof (story 3-1). Ignored unless `emit_usage > 0`.
    replay_usage: bool,
    /// Override the per-event input-token sentinel (story 3-1 C1/C2 boundary test).
    /// `None` = the fixed [`USAGE_INPUT_TOKENS`]. Lets a test emit a huge value (e.g.
    /// `u64::MAX`) to prove the storage boundary saturate-clamps rather than wraps.
    usage_input_tokens: Option<u64>,
    /// Override the per-event output-token sentinel (see `usage_input_tokens`).
    usage_output_tokens: Option<u64>,
    /// The per-event cached-token SUBSET stamped on each usage line (story 14-6).
    /// `0` = omit the optional field (the pre-14-6 line shape). MUST be
    /// `<=` the input sentinel for the engine's parser to accept the line
    /// (the INPUT-INCLUSIVE invariant — a larger cached count is malformed).
    usage_cached_tokens: u64,
    /// Emit ONE final usage sentinel line WITHOUT a trailing newline, then exit
    /// promptly (story 3-1 H1 under-count test). Its `sequence` is `emit_usage` (one
    /// past the batch), so the TERMINAL drain-on-reap must consume the newline-less
    /// tail or that event is stranded and lost.
    final_usage_no_newline: bool,
    /// ENGINE-OBSERVED mode (story 3-4): after announcing readiness, make `<N>`
    /// OpenAI-compatible completion requests to the `base_url` the engine injected
    /// into this process's environment (read from `OPENAI_BASE_URL` — the env var an
    /// `engine-observed` manifest maps `metering.base_url` onto). Each POST goes to
    /// `<base_url>/v1/chat/completions`; the engine's loopback listener forwards it
    /// to the test upstream stub and skims the `usage` out of the response. `0` =
    /// no observed calls (the default; existing self-reported tests unaffected).
    observed_calls: u64,
    /// The STREAMING sibling (story 12-2): `<N>` completion POSTs whose body sets
    /// `"stream": true` (the agent carries no `stream_options` of its own — the
    /// engine's listener injects `include_usage`). `0` = none (the default).
    observed_stream_calls: u64,
    /// The `Authorization: Bearer <value>` the observed calls carry (story 3-4
    /// no-leak test): a sentinel API key the proxy must relay UPSTREAM faithfully but
    /// leak into NONE of ktesio's surfaces. `None` = no auth header sent.
    observed_auth: Option<String>,
    /// Echo each stdin line to stdout as `stdin: <line>` (story 4-1). `false` =
    /// no echo thread (the default; existing tests that never write to stdin
    /// are unaffected).
    echo_stdin: bool,
    /// Synchronously block reading ONE stdin line BEFORE announcing
    /// readiness (story 4-1 fix pass, HIGH finding). `false` = no sniff (the
    /// default; existing tests are unaffected).
    sniff_stdin_at_startup: bool,
}

/// The FIXED token sentinels every emitted usage event carries (story 3-1), so a
/// test asserting the ledger total is an exact-match (`N * INPUT` etc.), never a
/// fuzzy range. Kept small + memorable.
const USAGE_INPUT_TOKENS: u64 = 10;
const USAGE_OUTPUT_TOKENS: u64 = 20;

/// Format ONE `KTESIO_USAGE {json}` self-reported usage sentinel line (story 3-1).
///
/// MUST match the engine's `hekma_engine::ports::parse_usage_line` convention:
/// the prefix `KTESIO_USAGE ` + a JSON object with snake_case `sequence`,
/// `input_tokens`, `output_tokens` — plus, since story 14-6, the OPTIONAL
/// `cached_tokens` subset (emitted ONLY when non-zero, so a cache-miss line
/// stays byte-identical to the pre-14-6 convention; the engine's parser reads
/// an absent field as known-zero). This binary cannot depend on the engine, so it
/// re-implements the shared shape in pure `std`; the engine's
/// `format_and_parse_round_trip_agree_on_the_convention` test guards the two
/// against drift. NO OS-cfg — the line is identical text on every OS.
fn usage_line(sequence: u64, input_tokens: u64, output_tokens: u64, cached_tokens: u64) -> String {
    if cached_tokens > 0 {
        format!(
            "KTESIO_USAGE {{\"sequence\":{sequence},\"input_tokens\":{input_tokens},\"output_tokens\":{output_tokens},\"cached_tokens\":{cached_tokens}}}"
        )
    } else {
        format!(
            "KTESIO_USAGE {{\"sequence\":{sequence},\"input_tokens\":{input_tokens},\"output_tokens\":{output_tokens}}}"
        )
    }
}

#[cfg(not(tarpaulin_include))]
fn parse() -> Opts {
    let mut exit_fast = None;
    let mut spawn_child = false;
    // Default "loop forever": a very long lingering time. A graceful stop kills
    // it via signal/job long before this elapses; this is only the self-exit
    // fallback so a stray test never leaks a truly immortal process.
    let mut linger = Duration::from_secs(3600);
    let mut marker = None;
    let mut dump = None;
    let mut heartbeat = None;
    let mut heartbeat_stderr = None;
    let mut crash_after = None;
    let mut crash_with = 1;
    let mut crash_times = None;
    let mut crash_state = None;
    let mut emit_usage = 0;
    let mut replay_usage = false;
    let mut usage_input_tokens = None;
    let mut usage_output_tokens = None;
    let mut usage_cached_tokens = 0;
    let mut final_usage_no_newline = false;
    let mut observed_calls = 0;
    let mut observed_stream_calls = 0;
    let mut observed_auth = None;
    let mut echo_stdin = false;
    let mut sniff_stdin_at_startup = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exit-fast" => {
                let code = args.next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
                exit_fast = Some(code);
            }
            "--emit-usage" => {
                if let Some(n) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    emit_usage = n;
                }
            }
            "--replay-usage" => replay_usage = true,
            "--usage-input-tokens" => {
                if let Some(n) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    usage_input_tokens = Some(n);
                }
            }
            "--usage-output-tokens" => {
                if let Some(n) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    usage_output_tokens = Some(n);
                }
            }
            "--usage-cached-tokens" => {
                if let Some(n) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    usage_cached_tokens = n;
                }
            }
            "--final-usage-no-newline" => final_usage_no_newline = true,
            "--observed-calls" => {
                if let Some(n) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    observed_calls = n;
                }
            }
            "--observed-stream-calls" => {
                // Review round 2: a missing or non-numeric value must FAIL
                // FAST (stderr + exit 2) — silently treating it as 0 would
                // make a mistyped e2e fixture assert an empty ledger for the
                // wrong reason (the agent never streamed) instead of the
                // behavior under test.
                match args.next().and_then(|s| s.parse::<u64>().ok()) {
                    Some(n) => observed_stream_calls = n,
                    None => {
                        eprintln!(
                            "fake_agent: --observed-stream-calls requires a non-negative \
                             integer argument (got missing or non-numeric value)"
                        );
                        std::process::exit(2);
                    }
                }
            }
            "--observed-auth" => {
                if let Some(v) = args.next() {
                    observed_auth = Some(v);
                }
            }
            "--echo-stdin" => echo_stdin = true,
            "--sniff-stdin-at-startup" => sniff_stdin_at_startup = true,
            "--spawn-child" => spawn_child = true,
            "--linger-ms" => {
                if let Some(ms) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    linger = Duration::from_millis(ms);
                }
            }
            "--heartbeat-ms" => {
                if let Some(ms) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    heartbeat = Some(Duration::from_millis(ms));
                }
            }
            "--heartbeat-stderr-ms" => {
                if let Some(ms) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    heartbeat_stderr = Some(Duration::from_millis(ms));
                }
            }
            "--crash-after-ms" => {
                if let Some(ms) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    crash_after = Some(Duration::from_millis(ms));
                }
            }
            "--crash-with" => {
                if let Some(code) = args.next().and_then(|s| s.parse::<i32>().ok()) {
                    crash_with = code;
                }
            }
            "--crash-times" => {
                if let Some(n) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    crash_times = Some(n);
                }
            }
            "--crash-state" => {
                if let Some(path) = args.next() {
                    crash_state = Some(PathBuf::from(path));
                }
            }
            "--marker" => {
                if let Some(path) = args.next() {
                    marker = Some(PathBuf::from(path));
                }
            }
            "--dump" => {
                if let Some(path) = args.next() {
                    dump = Some(PathBuf::from(path));
                }
            }
            // Unknown args are ignored so a manifest can pass extra tokens.
            _ => {}
        }
    }

    // Story 3-4 no-leak refinement: prefer an ENV-delivered observed auth over the
    // `--observed-auth` flag. Delivering the key as a `secret:` config leaf (mapped
    // onto this env var) keeps the sentinel OUT of the persisted registration
    // snapshot — the resolved cleartext reaches this process only in its START-time
    // environment — so the no-leak sweep can cover the WHOLE Agent Home (the
    // registration snapshot `adapter.json` included). The flag stays a fallback for
    // any caller that still passes it literally (e.g. a manifest `[lifecycle.start]`
    // arg).
    if let Ok(env_auth) = std::env::var("FAKE_AGENT_OBSERVED_AUTH") {
        observed_auth = Some(env_auth);
    }

    Opts {
        exit_fast,
        spawn_child,
        linger,
        marker,
        dump,
        heartbeat,
        heartbeat_stderr,
        crash_after,
        crash_with,
        crash_times,
        crash_state,
        emit_usage,
        replay_usage,
        usage_input_tokens,
        usage_output_tokens,
        usage_cached_tokens,
        final_usage_no_newline,
        observed_calls,
        observed_stream_calls,
        observed_auth,
        echo_stdin,
        sniff_stdin_at_startup,
    }
}

#[cfg(not(tarpaulin_include))]
fn main() {
    let opts = parse();

    // Story 4-1 fix pass (HIGH finding, review of #79): BEFORE anything else
    // (before the exit-fast path, before announcing readiness), mimic a
    // common real-CLI "sniff for piped input at startup" idiom by
    // synchronously blocking on ONE stdin line. Under the fixed engine
    // behavior (stdin piped only when the declared Capability::Interaction
    // is Guaranteed/BestEffort), an adapter that declares no interaction
    // support gets Stdio::null() here, so this read returns immediately
    // (EOF) and startup proceeds normally; under the OLD unconditional-pipe
    // regression this would hang forever, since nothing would ever close or
    // write to the pipe.
    if opts.sniff_stdin_at_startup {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    // Immediate-exit path (AC2): exit before doing anything else.
    if let Some(code) = opts.exit_fast {
        // Still drop a marker if asked, so a test can prove it launched at all.
        write_marker(&opts.marker, "exit-fast");
        std::process::exit(code);
    }

    // Resolve whether THIS launch should self-crash. Default: honor
    // `--crash-after-ms` on EVERY launch (the unbounded crash-loop behavior). When
    // BOTH `--crash-times <N>` and `--crash-state <path>` are set, the self-crash is
    // BOUNDED to the first N launches via a tiny persisted counter that survives the
    // restart (a fresh process): read the count, and if it is already `>= N` clear
    // the crash so this (restarted) launch stays up; otherwise increment+persist and
    // keep the crash armed. This makes an `on-failure` restart test deterministic —
    // `--crash-times 1` crashes exactly once, then the restarted process lingers.
    let effective_crash_after = match (
        opts.crash_after,
        opts.crash_times,
        opts.crash_state.as_ref(),
    ) {
        (Some(after), Some(limit), Some(state_path)) => {
            let prior = read_crash_count(state_path);
            if prior >= limit {
                None // bound reached — this launch does not crash
            } else {
                write_crash_count(state_path, prior + 1);
                Some(after)
            }
        }
        // No bound (or no state file to persist it): crash on every launch.
        (other, _, _) => other,
    };

    // Optionally spawn a looping child in the same process group / Job (it
    // inherits both), to prove the no-survivor kill catches the whole tree.
    //
    // The child is DELIBERATELY never `wait()`ed on: it must keep running
    // independently until the supervisor's process-group (Unix) / Job Object
    // (Windows) kill reaps it, which is exactly what the no-survivor test
    // asserts. Waiting here would defeat the test. Hence the allow.
    #[allow(clippy::zombie_processes)]
    let child_pid = if opts.spawn_child {
        let exe = std::env::current_exe().expect("current exe");
        // The child lingers a long time and does NOT spawn its own child.
        let child = Command::new(exe)
            .arg("--linger-ms")
            .arg("3600000")
            .spawn()
            .expect("spawn child fake_agent");
        Some(child.id())
    } else {
        None
    };

    // Announce readiness. The pid lines let a test learn the parent + child pids.
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "fake_agent ready pid={}", std::process::id());
    if let Some(pid) = child_pid {
        let _ = writeln!(stdout, "child-pid={pid}");
    }
    let _ = stdout.flush();
    write_marker(&opts.marker, "ready");
    // Story 2-2: dump the received argv + environment so the config-mapping proof
    // can observe a mapped unified key that landed as a native FLAG (in the args)
    // or ENV var (in the environment), without racing on stdout capture.
    write_dump(&opts.dump);

    // Story 4-1: a dedicated thread reads stdin line-by-line and echoes each as
    // `stdin: <line>` to the captured log — the deterministic vehicle a test
    // polls after writing a known line via the engine's `send_input`. Runs
    // independent of the main loop/heartbeat; ends naturally on stdin EOF (the
    // pipe closes when the engine drops/closes it) or process exit.
    if opts.echo_stdin {
        thread::spawn(|| {
            let stdin = std::io::stdin();
            let mut out = std::io::stdout();
            for line in BufReader::new(stdin).lines() {
                match line {
                    Ok(line) => {
                        let _ = writeln!(out, "stdin: {line}");
                        let _ = out.flush();
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Story 3-1: emit self-reported usage sentinel lines (readiness-gated — AFTER
    // the ready line, so the instance has reached `running` and the engine's reaper
    // is ingesting). Monotonic `sequence` 0..N with fixed token sentinels; a small
    // pause between lines lets the ~250ms reaper cadence drain them. If asked, then
    // re-emit `sequence 0` once — a DELAYED/replayed batch the ledger must dedup
    // (AC-A no-double-count). The test waits for the KNOWN committed row count in the
    // DB, so this schedule is a nudge, not a timing dependency. Pure `std`, NO OS-cfg.
    let input_tokens = opts.usage_input_tokens.unwrap_or(USAGE_INPUT_TOKENS);
    let output_tokens = opts.usage_output_tokens.unwrap_or(USAGE_OUTPUT_TOKENS);
    let cached_tokens = opts.usage_cached_tokens;
    for sequence in 0..opts.emit_usage {
        let _ = writeln!(
            stdout,
            "{}",
            usage_line(sequence, input_tokens, output_tokens, cached_tokens)
        );
        let _ = stdout.flush();
        sleep(Duration::from_millis(20));
    }
    if opts.emit_usage > 0 && opts.replay_usage {
        let _ = writeln!(
            stdout,
            "{}",
            usage_line(0, input_tokens, output_tokens, cached_tokens)
        );
        let _ = stdout.flush();
    }

    // Story 3-4 (ENGINE-OBSERVED): make `observed_calls` OpenAI-compatible completion
    // requests to the injected `base_url` (read from OPENAI_BASE_URL — the env var the
    // engine-observed manifest maps `metering.base_url` onto). The engine's loopback
    // forward listener relays each to the upstream stub and skims `usage` into the
    // ledger. Readiness-gated (AFTER the ready line) + count-bounded, so the test waits
    // for the KNOWN committed observed-row count, not a wall clock. Pure `std` HTTP.
    if opts.observed_calls > 0 || opts.observed_stream_calls > 0 {
        // The base_url the engine injected. Absent → nothing to call (a
        // misconfiguration the test would catch as zero committed rows); announce it
        // to stderr as a diagnostic and skip (never crash).
        match std::env::var("OPENAI_BASE_URL") {
            Ok(base_url) if !base_url.trim().is_empty() => {
                for _ in 0..opts.observed_calls {
                    // Best-effort per call: a transport hiccup is skipped (the test
                    // asserts on committed rows, not on this loop). A small pause lets
                    // the ~250ms reaper drain the observed queue between calls.
                    let _ = post_completion(base_url.trim(), opts.observed_auth.as_deref());
                    sleep(Duration::from_millis(20));
                }
                // Story 12-2: the streaming variant — same endpoint, a `"stream": true`
                // body, and the SSE response discarded unread (the usage the engine
                // meters arrives in the terminal SSE frame).
                for _ in 0..opts.observed_stream_calls {
                    let _ =
                        post_streaming_completion(base_url.trim(), opts.observed_auth.as_deref());
                    sleep(Duration::from_millis(20));
                }
            }
            _ => {
                let _ = writeln!(
                    std::io::stderr(),
                    "fake_agent: --observed-calls set but OPENAI_BASE_URL is unset/empty"
                );
            }
        }
    }

    // Story 3-1 (H1): a Run's FINAL usage line, flushed WITHOUT a trailing newline,
    // must not be stranded when the process exits. First stay alive PAST the engine's
    // ~300ms startup readiness window (so `start` confirms `running` — an immediate
    // exit would instead be read as a launch failure and never reach the reaper).
    // Then emit ONE usage line WITHOUT a newline (`sequence = emit_usage`, one past
    // the batch, so the test counts it distinctly) and exit. Because the process then
    // dies with a half-line in the log, ONLY the engine's TERMINAL drain-on-reap can
    // rescue it — a MidRun drain (which stops at the last newline) would strand it.
    // `write!` (not `writeln!`) leaves NO trailing newline; flush so the bytes reach
    // the captured log before exit.
    if opts.final_usage_no_newline {
        sleep(Duration::from_millis(500));
        let _ = write!(
            stdout,
            "{}",
            usage_line(opts.emit_usage, input_tokens, output_tokens, cached_tokens)
        );
        let _ = stdout.flush();
        std::process::exit(0);
    }

    // Loop until we are killed, until the crash-after window elapses (story 1-6:
    // a simulated UNREQUESTED crash → non-zero exit), or until the linger window
    // elapses (the clean self-exit fallback). When a heartbeat interval is set,
    // print an incrementing `heartbeat <n>` line every interval and flush — while
    // SIGSTOP'd the whole process freezes, so the captured log stops growing (the
    // story-1-5 observable-suspension proof); SIGCONT resumes it. With no
    // heartbeat this is a quiet short-poll sleep (unchanged 1-4 behavior). A
    // short poll keeps the process responsive to signals in all modes.
    let start = Instant::now();
    let deadline = start + opts.linger;
    let crash_deadline = effective_crash_after.map(|d| start + d);
    let mut beats: u64 = 0;
    let mut next_beat = opts.heartbeat.map(|interval| Instant::now() + interval);
    // Story 4-2: an INDEPENDENT stderr counter/interval (own `beats`, own
    // `next_beat`) so both streams can heartbeat simultaneously without
    // sharing state — a test combining both flags gets deterministic,
    // independently-countable content on stdout AND stderr.
    let mut stderr = std::io::stderr();
    let mut stderr_beats: u64 = 0;
    let mut next_stderr_beat = opts
        .heartbeat_stderr
        .map(|interval| Instant::now() + interval);
    while Instant::now() < deadline {
        // Story 1-6: after the crash-after window, exit non-zero (a crash the
        // supervisor's reaper detects, firing the Restart Policy). Checked after
        // readiness was announced above, so the instance reaches `running` first.
        if let Some(due) = crash_deadline {
            if Instant::now() >= due {
                let _ = writeln!(stdout, "crashing with code {}", opts.crash_with);
                let _ = stdout.flush();
                std::process::exit(opts.crash_with);
            }
        }
        if let (Some(interval), Some(due)) = (opts.heartbeat, next_beat) {
            if Instant::now() >= due {
                let _ = writeln!(stdout, "heartbeat {beats}");
                let _ = stdout.flush();
                beats += 1;
                next_beat = Some(due + interval);
            }
        }
        if let (Some(interval), Some(due)) = (opts.heartbeat_stderr, next_stderr_beat) {
            if Instant::now() >= due {
                let _ = writeln!(stderr, "stderr-heartbeat {stderr_beats}");
                let _ = stderr.flush();
                stderr_beats += 1;
                next_stderr_beat = Some(due + interval);
            }
        }
        sleep(Duration::from_millis(25));
    }
    // Lingered the whole window without being killed: exit cleanly.
    std::process::exit(0);
}

/// Read the cross-restart launch-crash counter (see `--crash-times`). A missing,
/// empty, or unparsable file reads as `0` (this is the first launch) — best-effort,
/// never a panic, so a filesystem hiccup degrades to "crash on this launch" rather
/// than crashing the harness. Pure `std`, NO OS-cfg.
#[cfg(not(tarpaulin_include))]
fn read_crash_count(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Persist the cross-restart launch-crash counter (see `--crash-times`).
/// Best-effort: a write failure just means the next launch re-reads the old value,
/// so the bound might crash one extra time — never a panic. Pure `std`, NO OS-cfg.
#[cfg(not(tarpaulin_include))]
fn write_crash_count(path: &Path, count: u64) {
    let _ = std::fs::write(path, count.to_string());
}

/// Best-effort write of a one-line marker file so tests can confirm startup
/// without racing on stdout capture.
#[cfg(not(tarpaulin_include))]
fn write_marker(path: &Option<PathBuf>, phase: &str) {
    if let Some(path) = path {
        let _ = std::fs::write(
            path,
            format!("fake_agent {phase} pid={}\n", std::process::id()),
        );
    }
}

/// Make ONE minimal OpenAI-compatible completion POST to `<base_url>/v1/chat/completions`
/// (story 3-4 engine-observed mode). Pure `std` HTTP/1.1 over a raw `TcpStream` — NO
/// dependency, NO OS-cfg. Sends a tiny JSON body, reads (and discards) the response.
/// Best-effort: any error is returned to the caller which skips it (the test asserts
/// on committed ledger rows, not on this call succeeding byte-for-byte). The `base_url`
/// is `http://127.0.0.1:<port>` (the engine's loopback listener); we parse the
/// host:port out of it, connect, and speak the minimal HTTP/1.1 a proxy relays.
#[cfg(not(tarpaulin_include))]
fn post_completion(base_url: &str, auth: Option<&str>) -> std::io::Result<()> {
    use std::io::Read;
    use std::net::TcpStream;

    // Parse `http://<host>:<port>` → the `host:port` authority (v1 the injected URL
    // is always http loopback with an explicit port). Strip the scheme + any path.
    let authority = base_url
        .strip_prefix("http://")
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("");
    if authority.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty authority",
        ));
    }

    let body = br#"{"model":"gpt-observed","messages":[{"role":"user","content":"hi"}]}"#;
    // An optional `Authorization: Bearer <key>` header (the no-leak sentinel): the
    // proxy must relay it UPSTREAM faithfully but leak it into none of ktesio's
    // surfaces. Included verbatim in the request head when set.
    let auth_header = match auth {
        Some(key) => format!("Authorization: Bearer {key}\r\n"),
        None => String::new(),
    };
    // A minimal, correct HTTP/1.1 request: the path the OpenAI client uses, Host, a
    // JSON content-type, an explicit content-length, and Connection: close so the
    // upstream/proxy closes the socket after one response (simplest read loop).
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {authority}\r\n\
         {auth_header}\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len(),
    );

    let mut stream = TcpStream::connect(authority)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    // Read the whole response and discard it (the engine skims `usage` on its side;
    // the agent just needs a faithful response, which we do not inspect here).
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink);
    Ok(())
}

/// The story-12-2 STREAMING sibling of [`post_completion`]: ONE minimal
/// OpenAI-compatible completion POST whose body sets `"stream": true` — and,
/// deliberately, NO `stream_options` of its own (the engine's listener injects
/// `include_usage`; the agent must not carry one or the injection is skipped).
/// The SSE response is drained and discarded like any real streaming client
/// would. Pure `std`, NO OS-cfg; same best-effort contract as the sibling.
#[cfg(not(tarpaulin_include))]
fn post_streaming_completion(base_url: &str, auth: Option<&str>) -> std::io::Result<()> {
    use std::io::Read;
    use std::net::TcpStream;

    // The same authority parse the sibling uses (`http://<host>:<port>` → authority).
    let authority = base_url
        .strip_prefix("http://")
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("");
    if authority.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty authority",
        ));
    }

    // The streaming request shape: `"stream": true`, NO stream_options (the
    // engine's injection is what adds it).
    let body =
        br#"{"model":"gpt-observed","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let auth_header = match auth {
        Some(key) => format!("Authorization: Bearer {key}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {authority}\r\n\
         {auth_header}\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len(),
    );

    let mut stream = TcpStream::connect(authority)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink);
    Ok(())
}

/// Best-effort write of the received argv + environment to the `--dump` file
/// (story 2-2). One `arg=<token>` line per argv entry (including argv[0]), then
/// one `env=<KEY>=<VALUE>` line per environment variable. The config-mapping proof
/// greps this for a mapped FLAG (an `arg=--model` / `arg=<value>` pair) or ENV var
/// (an `env=MODEL=<value>` line). Best-effort so it never fails the process.
#[cfg(not(tarpaulin_include))]
fn write_dump(path: &Option<PathBuf>) {
    let Some(path) = path else { return };
    let mut body = String::new();
    for arg in std::env::args() {
        body.push_str("arg=");
        body.push_str(&arg);
        body.push('\n');
    }
    for (key, value) in std::env::vars() {
        body.push_str("env=");
        body.push_str(&key);
        body.push('=');
        body.push_str(&value);
        body.push('\n');
    }
    let _ = std::fs::write(path, body);
}
