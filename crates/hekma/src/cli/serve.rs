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

/// The parsed `hekma serve` flags (story 21-4 grew the list past a plain
/// argument list: TLS, the hidden auto mode).
#[derive(Default)]
pub struct ServeFlags {
    /// `--stop-window <secs>`.
    pub stop_window: Option<u64>,
    /// `--metering-listen <port>`.
    pub metering_listen: u16,
    /// `--no-identity-headers` (story 23-1): send neither
    /// `x-hekma-run-id` nor `x-hekma-labels` to the model provider.
    pub no_identity_headers: bool,
    /// `--proxy-allowed-hosts <name,...>` (story 22-2).
    pub proxy_allowed_hosts: Vec<String>,
    /// `--proxy-allowed-origins <origin,...>` (story 22-2).
    pub proxy_allowed_origins: Vec<String>,
    /// `--state-dir <dir>`.
    pub state_dir: Option<String>,
    /// `--listen <addr>`.
    pub listen: Option<std::net::SocketAddr>,
    /// `--bearer-secret <name>`.
    pub bearer_secret: Option<String>,
    /// `--log-capture <mode>`.
    pub log_capture: Option<hekma_engine::LogCaptureMode>,
    /// `--otlp-endpoint <url>`.
    pub otlp_endpoint: Option<String>,
    /// `--otlp-headers <spec>`.
    pub otlp_headers: Option<String>,
    /// `--otlp-flush-interval <secs>`.
    pub otlp_flush_interval: Option<u64>,
    /// `--tls-cert <pem>` (story 21-4).
    pub tls_cert: Option<String>,
    /// `--tls-key <pem>` (story 21-4).
    pub tls_key: Option<String>,
    /// The hidden `--autostart` (story 21-4): the background host a CLI
    /// write starts.
    pub autostart: bool,
    /// The hidden `--idle-window <secs>` (story 21-4).
    pub idle_window: Option<u64>,
}

/// `hekma serve [--stop-window <secs>] [--metering-listen <port>]
/// [--state-dir <dir>] [--listen <addr> --bearer-secret <name>
/// [--tls-cert <pem> --tls-key <pem>]]` — serve until SIGTERM/SIGINT, then
/// drain.
///
/// The Control-API flags pass the parsed address + the secret NAME straight
/// through (the host owns the resolution; this file stays sync and free of
/// async/secret tokens — the embed-clean audit holds). The mutual
/// requires-between-flags is ALSO enforced in the host (`ServeError::
/// InvalidOptions`) so a library caller gets the same typed refusal a CLI
/// user gets from clap — and the host alone refuses a non-loopback `--listen`
/// without TLS (exit 2, before anything binds).
pub fn run(flags: ServeFlags) -> Result<(), Box<dyn std::error::Error>> {
    // The OTLP flags parse through the ONE site both surfaces share (the
    // host bin's hand-rolled loop calls the same `OtlpOptions::from_flags`);
    // a malformed combination (bad scheme, out-of-bounds interval, headers
    // without an endpoint) is the Usage-class refusal, same as any bad flag
    // value. Header VALUES never enter an error or a log (the redacting
    // newtype holds them).
    let otlp = hekma_host::OtlpOptions::from_flags(
        flags.otlp_endpoint,
        flags.otlp_headers.as_deref(),
        flags.otlp_flush_interval,
    )
    .map_err(|message| ServeOptionsInvalid { message })?;
    let options = hekma_host::ServeOptions {
        state_base: flags.state_dir.map(PathBuf::from),
        stop_window: flags.stop_window,
        metering_listen_port: flags.metering_listen,
        identity_headers: !flags.no_identity_headers,
        proxy_allowed_hosts: proxy_allowed_hosts(flags.proxy_allowed_hosts)
            .map_err(|message| ServeOptionsInvalid { message })?,
        proxy_allowed_origins: proxy_allowed_origins(flags.proxy_allowed_origins)
            .map_err(|message| ServeOptionsInvalid { message })?,
        control_listen: flags.listen,
        bearer_secret: flags.bearer_secret,
        // Story 17-4: the capture posture — `None` (no flag) means the
        // engine default (`full`, byte-identical to 17-3); the parsed flag
        // value overrides. An UNPARSEABLE value never reaches here: clap's
        // value parser refused at parse time.
        log_capture: flags.log_capture.unwrap_or_default(),
        // The stderr default: host + engine notices interleave on stderr,
        // byte-compatible with every other diagnostic surface.
        diagnostics: None,
        otlp,
        tls_cert: flags.tls_cert.map(PathBuf::from),
        tls_key: flags.tls_key.map(PathBuf::from),
        autostart: flags.autostart,
        // `control.json` records THIS build's version, so a CLI from another
        // build notes the skew.
        build_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        autostart_idle_override: flags.idle_window.map(std::time::Duration::from_secs),
        set_token_ttl_override: None,
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

/// `--proxy-allowed-hosts` (story 22-2): each a bare host name or address,
/// with an optional `:port` — what a `Host` header carries. Trimmed;
/// empty entries dropped; anything with a scheme, path or space refused.
fn proxy_allowed_hosts(values: Vec<String>) -> Result<Vec<String>, String> {
    let mut hosts = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.contains("://")
            || value.contains('/')
            || value.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "--proxy-allowed-hosts takes host names as a Host header carries them (for \
                 example proxy.internal or proxy.internal:8478), not '{value}'"
            ));
        }
        hosts.push(value.to_ascii_lowercase());
    }
    Ok(hosts)
}

/// `--proxy-allowed-origins` (story 22-2): each an origin as a browser
/// sends it, `scheme://host[:port]` with no path. Trimmed; empty entries
/// dropped.
fn proxy_allowed_origins(values: Vec<String>) -> Result<Vec<String>, String> {
    let mut origins = Vec::new();
    for value in values {
        let value = value.trim().trim_end_matches('/');
        if value.is_empty() {
            continue;
        }
        let valid = value
            .split_once("://")
            .is_some_and(|(scheme, rest)| {
                matches!(scheme, "http" | "https")
                    && !rest.is_empty()
                    && !rest.contains('/')
                    && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
            });
        if !valid {
            return Err(format!(
                "--proxy-allowed-origins takes origins as a browser sends them (for example \
                 https://app.example or http://localhost:3000), not '{value}'"
            ));
        }
        origins.push(value.to_ascii_lowercase());
    }
    Ok(origins)
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_guard_flags_take_host_names_and_bare_origins_only() {
        assert_eq!(
            proxy_allowed_hosts(vec![" Proxy.Internal ".into(), "".into(), "h:8478".into()]),
            Ok(vec!["proxy.internal".to_string(), "h:8478".to_string()])
        );
        for bad in ["http://h", "h/x", "a b"] {
            assert!(proxy_allowed_hosts(vec![bad.into()]).is_err(), "{bad}");
        }
        assert_eq!(
            proxy_allowed_origins(vec!["https://App.Example/".into(), " ".into()]),
            Ok(vec!["https://app.example".to_string()])
        );
        for bad in ["app.example", "ftp://x", "https://x/path", "https://", "null"] {
            assert!(proxy_allowed_origins(vec![bad.into()]).is_err(), "{bad}");
        }
    }
    use super::*;

    #[test]
    fn a_half_configured_control_api_is_the_usage_class_refusal() {
        // The class split (review finding 8), pinned IN-PROCESS: the host
        // validates its options BEFORE anything is touched, so the refusal
        // returns immediately — `--listen` without its bearer secret is the
        // ServeOptionsInvalid arm (Usage class, exit 2 at the classifier),
        // never a startup failure, and the remediation is named verbatim.
        let err = run(ServeFlags {
            // The state base is never reached: validation fires first.
            listen: Some("127.0.0.1:18499".parse().unwrap()),
            // The bearer secret — deliberately absent.
            ..ServeFlags::default()
        })
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

    #[test]
    fn a_non_loopback_listen_without_tls_is_the_usage_class_refusal() {
        // Story 21-4 (AD-20 amended 2026-10-30): beyond loopback the Control
        // API requires TLS — refused before anything binds or opens.
        let err = run(ServeFlags {
            listen: Some("0.0.0.0:18499".parse().unwrap()),
            bearer_secret: Some("NEVER_RESOLVED".into()),
            ..ServeFlags::default()
        })
        .expect_err("a non-loopback listen without TLS refuses");
        let mapped = err
            .downcast_ref::<ServeOptionsInvalid>()
            .expect("the InvalidOptions arm maps to the Usage class");
        assert!(mapped.message.contains("--tls-cert"), "{}", mapped.message);
    }
}
