//! `hekma serve` (story 17-2): the long-running Service Host, CLI-first.
//!
//! A plain BLOCKING call into `hekma_host::serve` — the HOST owns the tokio
//! runtime (the same internal-runtime shape the engine's blocking facade
//! already embodies), so this file carries NO async/tokio tokens (the
//! embed-clean audit pins that) and `crates/hekma/Cargo.toml` gains
//! `hekma-host`, never tokio (the boundary gate pins that: `hekma → host`
//! is the ratified AD-2 amendment edge).
//!
//! Output discipline (AD-12): the host's notices (started/state root, signal
//! receipt, per-instance stop outcomes, shutdown complete) are DIAGNOSTICS —
//! `[hekma] `-prefixed lines on STDERR, interleaved with the engine's own
//! diagnostics on the same stream. Stdout stays silent while serving: the
//! command's "result" is the long-running process itself.
//!
//! Refusals map onto the frozen exit-code table with NO new number (DC-4),
//! split by class per review finding 8: the state root already served
//! (`ServeError::StateDirLocked`) and an invalid option value
//! (`ServeError::InvalidOptions`) are Usage-class refusals — exit 2, via
//! `exit_code::classify`'s `ServeStateDirBusy`/`ServeOptionsInvalid` arms;
//! a host that cannot start (`ServeError::Startup` — state-dir creation,
//! lock I/O, engine open, runtime build, signal registration) is an
//! environment/system failure — exit 1 via `ServeStartupFailed`. The
//! shutdown drain ALWAYS exits 0 — a failed per-instance stop is surfaced
//! in the stream, not failed as a process exit (the drain ran; the host's
//! contract was kept).

use std::path::PathBuf;

use crate::error::{ServeOptionsInvalid, ServeStartupFailed, ServeStateDirBusy};

/// `hekma serve [--stop-window <secs>] [--metering-listen <port>]
/// [--state-dir <dir>] [--listen <addr> --bearer-secret <name>]` — serve
/// until SIGTERM/SIGINT, then drain.
///
/// The Control-API flags pass the parsed address + the secret NAME straight
/// through (the host owns the resolution; this file stays sync and free of
/// async/secret tokens — the embed-clean audit holds). The mutual
/// requires-between-flags is ALSO enforced in the host (`ServeError::
/// InvalidOptions`) so a library caller gets the same typed refusal a CLI
/// user gets from clap.
pub fn run(
    stop_window: Option<u64>,
    metering_listen: u16,
    state_dir: Option<String>,
    listen: Option<std::net::SocketAddr>,
    bearer_secret: Option<String>,
    log_capture: Option<hekma_engine::LogCaptureMode>,
) -> Result<(), Box<dyn std::error::Error>> {
    let options = hekma_host::ServeOptions {
        state_base: state_dir.map(PathBuf::from),
        stop_window,
        metering_listen_port: metering_listen,
        control_listen: listen,
        bearer_secret,
        // Story 17-4: the capture posture — `None` (no flag) means the
        // engine default (`full`, byte-identical to 17-3); the parsed flag
        // value overrides. An UNPARSEABLE value never reaches here: clap's
        // value parser refused at parse time.
        log_capture: log_capture.unwrap_or_default(),
        // The stderr default: host + engine notices interleave on stderr,
        // byte-compatible with every other diagnostic surface.
        diagnostics: None,
    };
    hekma_host::serve(options).map_err(|err| {
        // The typed refusal carries its full holder-naming/remediation text
        // (from the host's Display); the CLI diagnostic wraps it verbatim —
        // one surfacing, no re-wording. Class split per review finding 8:
        // the held-lock refusal and an invalid option are usage (exit 2),
        // a startup failure is environment/system (exit 1).
        match err {
            hekma_host::ServeError::StateDirLocked { .. } => Box::new(ServeStateDirBusy {
                message: err.to_string(),
            })
                as Box<dyn std::error::Error>,
            hekma_host::ServeError::InvalidOptions { .. } => Box::new(ServeOptionsInvalid {
                message: err.to_string(),
            }),
            hekma_host::ServeError::Startup { .. } => Box::new(ServeStartupFailed {
                message: err.to_string(),
            }),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_configured_control_api_is_the_usage_class_refusal() {
        // The class split (review finding 8), pinned IN-PROCESS: the host
        // validates its options BEFORE anything is touched, so the refusal
        // returns immediately — `--listen` without its bearer secret is the
        // ServeOptionsInvalid arm (Usage class, exit 2 at the classifier),
        // never a startup failure, and the remediation is named verbatim.
        let err = run(
            None,
            0,
            None, // the state base is never reached: validation fires first
            Some("127.0.0.1:18499".parse().unwrap()),
            None, // the bearer secret — deliberately absent
            None,
        )
        .expect_err("a half-configured API refuses");
        let mapped = err
            .downcast_ref::<ServeOptionsInvalid>()
            .expect("the InvalidOptions arm maps to the Usage class");
        assert!(
            mapped.message.contains("--bearer-secret"),
            "the remediation is named: {}",
            mapped.message
        );
    }
}
