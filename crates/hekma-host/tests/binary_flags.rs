//! Integration tests for the `hekma-host` BINARY's arg loop (story 17-4,
//! review finding 17): `--log-capture` parity with `hekma serve` — the same
//! flag, the same three values, the same parse refusal — proven against the
//! REAL binary (`CARGO_BIN_EXE_hekma-host`), so the 17-5 container binary
//! can reach every capture posture, not a hardcoded Full.
//!
//! The parse-refusal pins run everywhere. The end-to-end OFF leg (the flag
//! lands in a serving engine and `/v0/version` self-describes it)
//! RUNTIME-SKIPS on Windows via the house OS-id check (no cfg attributes —
//! the boundary gate confines conditional code to backends/): it shuts the
//! child down with POSIX `kill -TERM`, and the Windows console-event
//! harness is the recorded deferred-work gap (the serve CLI e2e's
//! disclosure — the same posture applies here).

use std::process::Command;
use std::time::{Duration, Instant};

/// The house runtime-skip check (the serve CLI e2e's idiom): no cfg
/// attributes in this file — the OS is data, decided at runtime.
fn is_windows() -> bool {
    hekma_engine::OsId::current() == hekma_engine::OsId::Windows
}

/// The HTTP client (the control_api suite's idiom: statuses are data, not
/// errors, and every wait is bounded).
fn client() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// The binary's path (the integration-test convention — the env var is set
/// for integration tests on every OS).
fn hekma_host_bin() -> &'static str {
    env!("CARGO_BIN_EXE_hekma-host")
}

#[test]
fn an_unparseable_log_capture_value_is_a_usage_refusal_naming_the_values() {
    // The parse pin: an invalid mode is a usage error (exit 2) whose output
    // names the accepted values — the SAME refusal shape `hekma serve`'s
    // clap parser gives (review finding 17: one surface to learn).
    let output = Command::new(env!("CARGO_BIN_EXE_hekma-host"))
        .args(["--log-capture", "sometimes"])
        .output()
        .expect("the binary must run");
    assert_eq!(
        output.status.code(),
        Some(2),
        "an invalid mode is a usage refusal: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--log-capture needs one of"),
        "the refusal names the expectation: {stderr}"
    );
    assert!(
        stderr.contains("full") && stderr.contains("metadata") && stderr.contains("off"),
        "the accepted values are named: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.is_empty(),
        "a usage error never writes stdout (help is the only stdout product): {stdout:?}"
    );
}

#[test]
fn a_missing_log_capture_value_is_a_usage_refusal() {
    let output = Command::new(env!("CARGO_BIN_EXE_hekma-host"))
        .arg("--log-capture")
        .output()
        .expect("the binary must run");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--log-capture needs one of"),
        "a valueless flag refuses the same way: {stderr}"
    );
}

#[test]
fn the_log_capture_flag_lands_in_the_serving_engine_and_the_version_says_so() {
    if is_windows() {
        return; // POSIX `kill -TERM` shutdown; on Windows the same flag path
                // is exercised by the parse-refusal pins above and by the
                // library-level serve suite — the real-console-event e2e
                // is the recorded deferred-work gap (the serve CLI e2e's
                // disclosure — the same posture applies here).
    }
    // The end-to-end parity leg: `hekma-host --log-capture off` serves with
    // capture OFF, and `/v0/version`'s self-description names it (review
    // finding 21's field, reached through the BINARY's flag).
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // The secrets file the bearer resolver reads (the API is on).
    let secrets = base.join(hekma_engine::paths::SECRETS_FILE);
    std::fs::write(&secrets, "CONTROL_BEARER = \"test-bearer-value\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();

    // An OS-ephemeral port for the API (bind-and-release; the classical
    // small race is acceptable in a hermetic test).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut child = Command::new(hekma_host_bin())
        .args([
            "--state-dir",
            base.to_str().unwrap(),
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--bearer-secret",
            "CONTROL_BEARER",
            "--log-capture",
            "off",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary spawns");

    // Poll /v0/version until the API is up (the child reports listening on
    // stderr too — the status poll is the simpler latch here).
    let client = client();
    let deadline = Instant::now() + Duration::from_secs(15);
    let body = loop {
        match client
            .get(format!("http://127.0.0.1:{port}/v0/version"))
            .header("Authorization", "Bearer test-bearer-value")
            .call()
        {
            Ok(mut response) if response.status().as_u16() == 200 => {
                break response
                    .body_mut()
                    .read_to_string()
                    .expect("the body reads");
            }
            _ => {
                assert!(
                    Instant::now() < deadline,
                    "the API never came up (child alive: {:?})",
                    child.try_wait().unwrap().is_none()
                );
                std::thread::sleep(Duration::from_millis(40));
            }
        }
    };
    let json: serde_json::Value = serde_json::from_str(&body).expect("the version body is JSON");
    assert_eq!(
        json["log_capture"], "off",
        "the binary's flag reached the serving engine's self-description: {json}"
    );

    // Clean shutdown: SIGTERM via the `kill` shell-out (the serve CLI e2e's
    // idiom — std has no signal-a-child API), then the drain exits 0 (the
    // host stops nothing — no agents — and exits cleanly).
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill shell-out");
    assert!(status.success(), "the kill shell-out failed");
    let deadline = Instant::now() + Duration::from_secs(60);
    let exit = loop {
        if let Some(exit) = child.try_wait().expect("try_wait") {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "the host did not exit after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(
        exit.code(),
        Some(0),
        "a signalled host drains and exits zero: {exit}"
    );
}
