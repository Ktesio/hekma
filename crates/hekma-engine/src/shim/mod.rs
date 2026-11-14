//! The Shim (story 24-1, spine AD-23): one small process per supervised
//! agent that owns its process — the group or Job, the pipes, the log
//! redirects — so the agent no longer lives or dies with the engine.
//!
//! This module is the Shim as a LIBRARY (`hekma shim` is a thin wrapper;
//! tests run it in-process):
//! * [`proto`] — `shim-rpc v1`: newline-delimited JSON frames, the version
//!   in the first message, redacting `Debug` on every frame;
//! * [`secret`] — the per-state-directory host key and each launch's
//!   derived connection secret (only its hash is ever stored);
//! * [`server`] — the single-controller server: it runs the existing per-OS
//!   process backend inside itself, buffers the child's exit while no engine
//!   is connected, and never relaunches, writes state.db or mints anything;
//! * [`client`] — the engine's side of a connection.
//!
//! The transports (a Unix domain socket in a `0700` directory with a
//! peer-uid check; the Windows named pipe follows) live under `backends/`,
//! the only home for OS-conditional code.

pub mod client;
pub mod proto;
pub mod secret;
pub mod server;

use std::time::Duration;

/// `shim-rpc` major version: a different major is never spoken.
pub const SHIM_PROTOCOL_MAJOR: u32 = 1;
/// `shim-rpc` minor version: the engine accepts its own and the previous.
pub const SHIM_PROTOCOL_MINOR: u32 = 1;

/// How long a Shim that is never asked to launch waits before exiting
/// (its engine died between spawning it and the launch request).
pub const SHIM_IDLE_LINGER: Duration = Duration::from_secs(60);

/// How long a Shim whose child exited keeps the exit for an engine to fetch
/// and acknowledge, before exiting itself.
pub const SHIM_EXIT_LINGER: Duration = Duration::from_secs(60 * 60);

/// The directory under the state directory that holds the Shims' sockets.
pub const SHIM_DIR: &str = "shims";

/// The socket a Shim for `launch_id` listens on: `<state>/shims/s-<12 hex>.sock`
/// — short, so it stays under `sun_path`'s limit whatever the state path.
pub fn socket_path(state_dir: &std::path::Path, launch_id: &str) -> std::path::PathBuf {
    let hash = secret::to_hex(&crate::sha256::digest(launch_id.as_bytes()));
    state_dir
        .join(SHIM_DIR)
        .join(format!("s-{}.sock", &hash[..12]))
}

/// Run a Shim on `socket` expecting the secret whose SHA-256 is
/// `expected_secret_hash` (hex): the `hekma shim` entry. Returns when a
/// linger ends it. On Linux the process becomes non-dumpable first.
pub fn run(
    socket: &std::path::Path,
    expected_secret_hash: &str,
) -> Result<server::ShimExit, String> {
    let hash = secret::from_hex(expected_secret_hash)
        .ok_or_else(|| "--expect-secret-hash must be 64 hex characters".to_string())?;
    let _ = crate::backends::harden_host_memory();
    let listener = crate::backends::shim_listen(socket)
        .map_err(|err| format!("the Shim could not listen on {}: {err}", socket.display()))?;
    Ok(server::serve(
        listener.as_ref(),
        server::ShimConfig::new(hash),
    ))
}

/// Connect to the Shim on `socket` and say hello with `secret`.
pub fn connect(
    socket: &std::path::Path,
    secret: &[u8; 32],
    bound: Duration,
) -> Result<(client::ShimClient, proto::ShimStatus), client::ShimClientError> {
    let stream = crate::backends::shim_connect(socket)?;
    client::ShimClient::hello(stream, secret, bound)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_socket_name_is_short_and_stable_per_launch() {
        let state = std::path::Path::new("/a/very/long/state/directory/path/that/goes/on");
        let one = super::socket_path(state, "launch-1");
        assert_eq!(one, super::socket_path(state, "launch-1"));
        assert_ne!(one, super::socket_path(state, "launch-2"));
        let name = one.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("s-") && name.ends_with(".sock") && name.len() == 19,
            "{name}"
        );
        assert!(super::run(std::path::Path::new("/nonexistent/x.sock"), "zz").is_err());
    }
}
