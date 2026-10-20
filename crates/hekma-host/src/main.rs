//! The `hekma-host` binary (story 17-2): a THIN wrapper over the library's
//! [`hekma_host::serve`] — the real operator surface is the `hekma serve`
//! CLI subcommand (CLI-first gate), and the full container/daemon surface
//! (flags, env wiring, health exposure) is story 17-5's. This bin exists so
//! the crate is the ratified "bin + lib" shape and a container image can
//! run the host without the `hekma` CLI present.
//!
//! Hand-rolled arg loop (no clap here — the crate's deps stay hekma-engine +
//! tokio + the axum set, the AD-2 tree discipline): `--help`/`-h`,
//! `--version`/`-V`, `--state-dir <dir>` (the SAME flag name and semantics as
//! `hekma serve`'s `--state-dir` — one surface to learn; review finding 19),
//! `--stop-window <secs>` (bounded 1..=86400 like every other surface,
//! finding 9), `--metering-listen <port>`, and — story 17-3 — `--listen
//! <addr>` + `--bearer-secret <name>` (the Control API; BOTH or NEITHER — a
//! half-configured API is a usage error, and an unparseable address is too).
//! Story 17-4 (review finding 17): `--log-capture <full|metadata|off>` —
//! the SAME flag and values as `hekma serve`, so the container binary can
//! reach every capture posture.
//! Anything else is a usage error:
//! the message AND the usage text go to STDERR (help is the only stdout
//! product; finding 19), exit 2. `ServeError` variants map onto the same
//! exit classes as the CLI (finding 8): the lock refusal and an invalid
//! option are usage (exit 2); a startup failure is IO/system (exit 1).
//! Both state-dir env overrides the engine honors (`HEKMA_STATE_DIR` and
//! the legacy `KTESIO_STATE_DIR`) are named in the help.

#[cfg(not(tarpaulin_include))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("hekma-host {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    let mut state_dir: Option<std::path::PathBuf> = None;
    let mut stop_window: Option<u64> = None;
    let mut metering_listen: u16 = 0;
    let mut control_listen: Option<std::net::SocketAddr> = None;
    let mut bearer_secret: Option<String> = None;
    let mut log_capture: Option<hekma_engine::LogCaptureMode> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                i += 1;
                match args.get(i) {
                    Some(dir) => state_dir = Some(std::path::PathBuf::from(dir)),
                    None => usage_error("--state-dir needs a directory argument"),
                }
            }
            "--stop-window" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    // The same inclusive bounds every surface honors
                    // (review finding 9): clap ranges on the CLI, this
                    // check here, `ServeError::InvalidOptions` in the lib.
                    Some(secs)
                        if (hekma_host::MIN_STOP_WINDOW_SECS
                            ..=hekma_host::MAX_STOP_WINDOW_SECS)
                            .contains(&secs) =>
                    {
                        stop_window = Some(secs)
                    }
                    _ => usage_error(&format!(
                        "--stop-window needs a whole number of seconds from {} to {}",
                        hekma_host::MIN_STOP_WINDOW_SECS,
                        hekma_host::MAX_STOP_WINDOW_SECS
                    )),
                }
            }
            "--metering-listen" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u16>().ok()) {
                    Some(port) => metering_listen = port,
                    None => usage_error("--metering-listen needs a port number (0-65535)"),
                }
            }
            "--listen" => {
                i += 1;
                match args
                    .get(i)
                    .and_then(|v| v.parse::<std::net::SocketAddr>().ok())
                {
                    Some(addr) => control_listen = Some(addr),
                    None => usage_error("--listen needs a socket address like 127.0.0.1:8477"),
                }
            }
            "--bearer-secret" => {
                i += 1;
                match args.get(i) {
                    // The NAME of the secret (env var or secrets-file key) —
                    // never the secret value itself. A FLAG-looking value
                    // (`--bearer-secret --listen ...`) is a swallowed
                    // neighbor, not a name — refuse with a usage error
                    // naming the expectation (review item 5) instead of
                    // "resolving" a secret literally named `--listen`.
                    Some(name) if !name.starts_with('-') => {
                        bearer_secret = Some(name.clone());
                    }
                    Some(_) => usage_error(
                        "--bearer-secret needs a secret NAME (the env var or secrets-file \
                         key), not a flag-looking value; e.g. `--bearer-secret \
                         CONTROL_BEARER`",
                    ),
                    None => usage_error("--bearer-secret needs a secret NAME"),
                }
            }
            "--log-capture" => {
                i += 1;
                // Story 17-4 (review finding 17): the SAME flag, values, and
                // parse refusal as `hekma serve` (one surface to learn) —
                // the 17-5 container binary must reach every capture
                // mode, not a hardcoded Full. The engine's FromStr names
                // the accepted values in its error (surfaced verbatim).
                match args
                    .get(i)
                    .and_then(|v| v.parse::<hekma_engine::LogCaptureMode>().ok())
                {
                    Some(mode) => log_capture = Some(mode),
                    None => {
                        usage_error("--log-capture needs one of \"full\", \"metadata\", or \"off\"")
                    }
                }
            }
            other => usage_error(&format!("unknown argument {other:?}")),
        }
        i += 1;
    }
    let options = hekma_host::ServeOptions {
        state_base: state_dir,
        stop_window,
        metering_listen_port: metering_listen,
        control_listen,
        bearer_secret,
        // Story 17-4 (review finding 17): the parsed flag (absent = the
        // engine's `full` default — the same single defaulting site as the
        // CLI, never a second one here).
        log_capture: log_capture.unwrap_or_default(),
        diagnostics: None,
    };
    match hekma_host::serve(options) {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            // The same variant → exit-class mapping the `hekma serve` CLI
            // uses (review finding 8): a held lock / invalid option is a
            // usage refusal (2); a startup failure is IO/system (1).
            let exit = match err {
                hekma_host::ServeError::StateDirLocked { .. }
                | hekma_host::ServeError::InvalidOptions { .. } => 2,
                hekma_host::ServeError::Startup { .. } => 1,
            };
            eprintln!("[hekma] {err}");
            std::process::exit(exit);
        }
    }
}

#[cfg(not(tarpaulin_include))]
fn print_usage() {
    println!("{}", usage_text());
}

/// The usage text, built ONCE — `--help` prints it to stdout, a usage error
/// prints the SAME text to stderr (review finding 19).
#[cfg(not(tarpaulin_include))]
fn usage_text() -> String {
    "hekma-host — the Hekma long-running Service Host\n\
     \n\
     USAGE:\n    \
         hekma-host [OPTIONS]\n\
     \n\
     OPTIONS:\n    \
         --state-dir <dir>        Serve this state directory (default: the platform\n    \
                                  data dir; the engine honors HEKMA_STATE_DIR and the\n    \
                                  legacy KTESIO_STATE_DIR)\n    \
         --stop-window <secs>     Graceful stop window for the shutdown drain, a\n    \
                                  whole number of seconds from 1 to 86400 (default:\n    \
                                  the engine's 30s)\n    \
         --metering-listen <port> Fixed loopback port for observed metering\n    \
                                  listeners (default: 0 = OS-ephemeral)\n    \
         --listen <addr>          Expose the v0 Control API on this address\n    \
                                  (e.g. 127.0.0.1:8477). Default: off\n    \
         --bearer-secret <name>   The NAME of the bearer secret guarding the\n    \
                                  Control API (an env var, or a key in the\n    \
                                  state dir's secrets file). Required with\n    \
                                  --listen; the API never serves unauthenticated\n    \
         --log-capture <mode>     The capture posture: full (default),\n    \
                                  metadata (hashed records at rest), or off\n    \
                                  (nothing captured). Same flag and values\n    \
                                  as `hekma serve`\n    \
         -h, --help               Print this help\n    \
         -V, --version            Print the version\n\
     \n\
     The full operator surface is `hekma serve --help`."
        .to_string()
}

/// A usage refusal: the message AND the usage text go to STDERR (stdout
/// stays clean for `--help`'s product — review finding 19), exit 2.
#[cfg(not(tarpaulin_include))]
fn usage_error(message: &str) -> ! {
    eprintln!("[hekma] {message}");
    eprintln!("{}", usage_text());
    std::process::exit(2);
}
