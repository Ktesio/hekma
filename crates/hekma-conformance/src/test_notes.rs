//! The uncaptured stderr NOTE helper (story 16-1, spec
//! `spec-16-1-skip-note-visibility`): the ONE escape hatch a test suite uses
//! to put a skip note on the operator's terminal even when libtest is
//! capturing.
//!
//! ## The problem it closes
//!
//! libtest captures the stdout AND stderr of every test and only replays a
//! test's captured stderr on FAILURE (or under `--nocapture`). The
//! real-agent smokes' skip notes are emitted by tests that then PASS — the
//! skip is the honest, expected outcome — so an `eprintln!`-emitted note is
//! captured and never shown: a plain `cargo test` run reports `ok` with the
//! surfaced-not-silent posture (AI-18) invisible (the 15-1 review's deferred
//! finding). `println!`/`eprintln!` cannot escape this: the capture sink
//! intercepts the MACROS (it swaps the thread-local output the macros print
//! through), not the file descriptors.
//!
//! ## The escape hatch — and its one discipline
//!
//! [`note_uncaptured`] writes the note straight to the process's REAL stderr
//! (file descriptor 2, via a `std::fs::File` constructed from the raw fd —
//! the Windows equivalent uses the raw handle). A `File` from a raw fd is
//! written directly by the OS; no macro, no thread-local sink, no buffering
//! sits between the bytes and the descriptor, so the note lands on the
//! terminal under a plain `cargo test` run. Under `--nocapture` the bytes are
//! identical to what `eprintln!` wrote before (same stream, same wording,
//! same trailing newline) — the story changed the CHANNEL, never the text.
//!
//! The `ManuallyDrop` discipline (the Design Note's construct → write →
//! forget): fd 2 belongs to the PROCESS, not to this function, so the
//! `File` wrapper must never close the descriptor on drop — the file is
//! wrapped in `std::mem::ManuallyDrop`, which releases the wrapper without
//! running `File`'s drop (a plain `std::mem::forget` would leak the wrapper
//! on an early panic; `ManuallyDrop` is the panic-safe spelling of the same
//! thing). Without it, the FIRST note would permanently close the process's
//! stderr and every later write (including libtest's own failure dumps) would
//! fail.
//!
//! ## Contract
//!
//! * **Best-effort, never panics.** A write error (a broken/closed stderr
//!   pipe, a full disk, whatever) is swallowed SILENTLY — `eprintln!` panics
//!   on a broken pipe, and a note must never be worth failing a test over.
//!   The test outcome is the truth; the note is the courtesy surface.
//! * **Skip notes only.** This helper exists for the smokes' skip paths.
//!   Failure diagnostics keep libtest's normal captured-then-replayed
//!   behavior — the capture is exactly what makes a failing test's context
//!   land next to the failure report. No capture-bypass for anything else.
//!
//! ## Role: test infrastructure, never a driving surface
//!
//! Like the rest of this crate ([`crate::test_support`]), this module is
//! test infrastructure: it writes bytes to a descriptor and offers no way to
//! drive, spawn, or observe an agent. The crate is dev-dependency-only
//! downstream (the AD-2 boundary note in the crate root applies unchanged),
//! and the helper is pure `std` — no new dependencies, no nightly features.

use std::fs::File;
use std::io::Write as _;
use std::mem::ManuallyDrop;

/// Write `msg` to the process's REAL stderr (fd 2), bypassing libtest's
/// capture of passing tests, with the same trailing newline `eprintln!` would
/// have added — the skip-note channel for tests whose skip paths PASS (the
/// real-agent smokes, story 16-1). Best-effort: write errors are swallowed
/// and the function never panics (a broken stderr pipe must not fail a
/// test). Under `--nocapture` the bytes are identical to `eprintln!(msg)`'s.
pub fn note_uncaptured(msg: &str) {
    // One write of note + newline (eprintln!'s byte shape for a plain
    // string): the payload is built first so a partial write can never
    // interleave the text and the newline with another thread's output.
    let mut payload = String::with_capacity(msg.len() + 1);
    payload.push_str(msg);
    payload.push('\n');
    write_real_stderr(payload.as_bytes());
}

/// The raw-descriptor write. cfg-gated per OS family because the raw
/// resource's TYPE differs (a file descriptor on Unix, a handle on Windows);
/// both arms are the same three steps — construct from the raw stderr,
/// write, let the `ManuallyDrop` release the wrapper without closing.
fn write_real_stderr(bytes: &[u8]) {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        // SAFETY: fd 2 is the process's stderr, owned by the process for its
        // whole lifetime (the runtime never closes it out from under us). The
        // `File` is a temporary VIEW of that descriptor: `ManuallyDrop`
        // guarantees the wrapper's drop never closes fd 2 (the construct →
        // write → forget discipline from the spec's Design Note).
        let mut stderr = ManuallyDrop::new(unsafe { File::from_raw_fd(2) });
        let _ = stderr.write_all(bytes);
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        // SAFETY: the Windows equivalent of the Unix arm. The real stderr
        // HANDLE is taken from `std::io::stderr()` (NOT the integer 2 — on
        // Windows that is the CRT fd number, not a `HANDLE`; constructing one
        // from it would yield an invalid handle whose writes fail silently).
        // `ManuallyDrop` guarantees the wrapper never closes the handle —
        // the process owns it.
        let raw = std::io::stderr().as_raw_handle();
        let mut stderr = ManuallyDrop::new(unsafe { File::from_raw_handle(raw) });
        let _ = stderr.write_all(bytes);
    }
    #[cfg(not(any(unix, windows)))]
    {
        // An unmodeled OS has no raw-stderr escape hatch here: degrade to the
        // ordinary (capturable) stream — the graceful-degradation posture,
        // and still never a panic.
        let _ = std::io::stderr().write_all(bytes);
    }
}

// ---------------------------------------------------------------------------
// Tests (the helper's pins: the pipe round-trip + the no-panic closed pipe)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// The env marker that turns a re-execution of THIS test binary into the
    /// note-writing child (the crash suites' re-exec-the-test-binary
    /// pattern). The marker distinguishes the child from the ordinary parent
    /// run so the same `#[test]` fn serves both roles.
    const ROUNDTRIP_CHILD_ENV: &str = "HEKMA_TEST_NOTE_UNCAPTURED_ROUNDTRIP_CHILD";
    const CLOSED_PIPE_CHILD_ENV: &str = "HEKMA_TEST_NOTE_UNCAPTURED_CLOSED_PIPE_CHILD";

    /// The child's fixed note tokens: each parent pipes ONLY its own child's
    /// stderr, so the tokens need no cross-run uniqueness — they only need to
    /// be strings no other stderr writer emits.
    const ROUNDTRIP_TOKEN: &str = "hekma-note-uncaptured-round-trip";
    /// The eprintln! CONTROL token: libtest captures it (the child test
    /// passes), so it must NEVER surface on the real stderr the parent reads.
    const CAPTURED_CONTROL_TOKEN: &str = "hekma-note-captured-must-not-appear";
    const CLOSED_PIPE_TOKEN: &str = "hekma-note-closed-pipe-probe";

    /// The full libtest name of a test in this module (stable under module
    /// renames: derived from `module_path!()`, not hardcoded). libtest
    /// registers LIB-crate tests WITHOUT the crate-name prefix while
    /// `module_path!()` carries it, so the leading segment is stripped.
    fn full_test_name(short: &str) -> String {
        let path = module_path!();
        let relative = path.split_once("::").map(|(_, rest)| rest).unwrap_or(path);
        format!("{relative}::{short}")
    }

    /// Re-execute THIS test binary running exactly one named test, with the
    /// given child marker env set and the given stderr plumbing. Stdout is
    /// nulled (libtest's own progress/summary output is not what the pins
    /// assert on) and the child's coverage profile is redirected to the
    /// scratch path ([`crate::helper_profile_env`]) so a re-exec'd child
    /// under tarpaulin never races the profraw merge (the #239 lesson; the
    /// same guard the crash suites apply).
    fn spawn_self_running(test: &str, child_env: &str, stderr: Stdio) -> std::process::Child {
        let exe = std::env::current_exe().expect("locate the running test executable");
        Command::new(exe)
            .args([full_test_name(test).as_str(), "--exact"])
            .env(child_env, "1")
            .env("LLVM_PROFILE_FILE", crate::helper_profile_env())
            // The child must run CAPTURED for the differential to hold: if
            // the parent was started with RUST_TEST_NOCAPTURE set (or any
            // --nocapture equivalent leaked into the env), the control
            // token would reach the pipe too and the round-trip pin would
            // fail spuriously. The pin tests capture behavior — force it.
            .env_remove("RUST_TEST_NOCAPTURE")
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("re-exec the test binary for the note child")
    }

    /// THE round-trip pin: a note written by a test that PASSES reaches the
    /// process's REAL stderr (the parent's pipe), while a sibling `eprintln!`
    /// marker stays inside libtest's capture sink. That differential IS the
    /// story: same process, same test, two channels — only the fd-2 write
    /// escapes.
    #[test]
    fn note_uncaptured_reaches_the_real_stderr_while_the_macro_stays_captured() {
        if std::env::var(ROUNDTRIP_CHILD_ENV).is_ok() {
            // The re-executed child: emit both tokens and pass. The macro's
            // bytes are captured (this test passes, so the capture is never
            // replayed); the helper's bytes must land on the real stderr the
            // parent is piping.
            eprintln!("{CAPTURED_CONTROL_TOKEN}");
            note_uncaptured(ROUNDTRIP_TOKEN);
            return;
        }
        let child = spawn_self_running(
            "note_uncaptured_reaches_the_real_stderr_while_the_macro_stays_captured",
            ROUNDTRIP_CHILD_ENV,
            Stdio::piped(),
        );
        let output = child.wait_with_output().expect("wait for the note child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "the note child must pass: status={output:?} stderr={stderr:?} stdout={stdout:?}"
        );
        assert!(
            stderr.contains(ROUNDTRIP_TOKEN),
            "the fd-2 write reached the REAL stderr of a PASSING test: stderr={stderr:?} \
             stdout={stdout:?}"
        );
        assert!(
            stderr.contains(&format!("{ROUNDTRIP_TOKEN}\n")),
            "the note carries eprintln's trailing newline: {stderr:?}"
        );
        assert!(
            !stderr.contains(CAPTURED_CONTROL_TOKEN),
            "the eprintln! control stayed in libtest's capture sink (it is NOT on the \
             real stderr): {stderr:?} stdout={stdout:?}"
        );
    }

    /// THE no-panic pin: after the pipe's read end is closed, every note
    /// write fails (`EPIPE` on Unix / `ERROR_BROKEN_PIPE` on Windows) and the
    /// helper must SWALLOW it — the child keeps running and passes. A panic
    /// (the `eprintln!` behavior on a broken pipe) or a `SIGPIPE` death would
    /// fail the child and this assertion with it.
    #[test]
    fn note_uncaptured_swallows_a_broken_stderr_pipe_and_never_panics() {
        if std::env::var(CLOSED_PIPE_CHILD_ENV).is_ok() {
            // The child: several notes over a window long enough that the
            // parent's read end is long gone — every write faces the closed
            // pipe and must be silently swallowed. libtest's own output goes
            // to stdout (nulled by the parent), so nothing else writes the
            // broken stderr.
            for _ in 0..8 {
                note_uncaptured(CLOSED_PIPE_TOKEN);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            return;
        }
        let mut child = spawn_self_running(
            "note_uncaptured_swallows_a_broken_stderr_pipe_and_never_panics",
            CLOSED_PIPE_CHILD_ENV,
            Stdio::piped(),
        );
        // Close the read end IMMEDIATELY (well inside the child's write
        // window): the child's writes then target a pipe with no reader.
        drop(child.stderr.take());
        let status = child.wait().expect("wait for the closed-pipe child");
        assert!(
            status.success(),
            "the helper swallowed the broken-pipe writes — a panic or signal death \
             would have failed the child: {status:?}"
        );
    }
}
