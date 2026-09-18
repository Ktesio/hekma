//! `hermes_shim` — the story 6-2 PATH-shim launcher.
//!
//! The engine's native `hermes` adapter declares a FIXED launch
//! (`exec = "hermes"`, `args = ["gateway", "run", "--external-supervisor"]`).
//! That argv is CONTRACT — tests cannot add flags to it. To exercise the real
//! start path end-to-end without a network, a test copies this tiny launcher to
//! `<tmp>/hermes<EXE_SUFFIX>` and prepends that directory to `PATH`, so the
//! declared exec resolves here. The shim:
//!
//! 1. reads its test script from the env var `HERMES_SHIM_ARGS`
//!    (split on ASCII spaces; no values with spaces are expressible — fine for
//!    every fake_agent flag), and
//! 2. re-execs [`hekma_conformance::fake_agent_bin`] with
//!    `[original args..., script args...]`.
//!
//! The original argv (the gateway-shaped contract launch) is forwarded FIRST so
//! a `--dump` artifact proves both halves: the fixed launch arrived verbatim,
//! AND which environment the engine injected (`env=HERMES_HOME=…`). A missing
//! `HERMES_SHIM_ARGS` means "forward and behave like plain fake_agent".
//!
//! // Pure std; NO OS-cfg (the OS-cfg CI gate allowlists only `backends/`).

// This whole binary runs only as a SPAWNED SUBPROCESS of the story 6-2
// integration test (`crates/hekma-engine/tests/hermes.rs`), which copies it to
// a temp dir and launches it via the engine's PATH resolution, so a coverage
// harness (tarpaulin) instrumenting the test process can never record its
// lines — the same rationale as `fake_agent`. Exclude it from coverage; its
// BEHAVIOR is proven by the spawning test, not by line coverage.
#[cfg(not(tarpaulin_include))]
fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(script) = std::env::var("HERMES_SHIM_ARGS") {
        argv.extend(
            script
                .split(' ')
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }
    let bin = hekma_conformance::fake_agent_bin();
    let err = std::process::Command::new(bin).args(&argv).status();
    let code = match err {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("hermes_shim: failed to launch fake_agent: {e}");
            1
        }
    };
    std::process::exit(code);
}
