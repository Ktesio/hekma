//! `hekma serve --metering-listen IP:PORT` and the proxy's TLS flags (story
//! 22-5) through the real binary: every refusal is exit 2 before anything
//! is created; `hekma binding show` prints the address clients on other
//! machines use when the serving host records one.
//!
//! Every OS: no agent process, no host left running.

mod helpers;

use helpers::{run_hekma_agent, TestContext};

#[test]
fn a_bind_beyond_loopback_without_tls_and_a_host_name_are_usage_refusals() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.join("state");
    for (args, expected) in [
        (
            vec!["serve", "--metering-listen", "0.0.0.0:0"],
            "without transport encryption",
        ),
        (
            vec!["serve", "--metering-listen", "proxy.example:8400"],
            "IP literal",
        ),
        (
            vec!["serve", "--proxy-tls-cert", "cert.pem"],
            "--proxy-tls-key",
        ),
        (
            vec![
                "serve",
                "--metering-listen",
                "0.0.0.0:0",
                "--proxy-tls-cert",
                "missing-cert.pem",
                "--proxy-tls-key",
                "missing-key.pem",
            ],
            "missing-cert.pem",
        ),
    ] {
        let run = run_hekma_agent(&args, &ctx.project_dir, &state_dir);
        assert_eq!(run.code, Some(2), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{args:?}: {}", run.stderr);
        assert!(!state_dir.exists(), "{args:?} created the state directory");
    }
    let run = run_hekma_agent(&["serve", "--help"], &ctx.project_dir, &state_dir);
    for flag in [
        "PORT|IP:PORT",
        "--proxy-tls-cert",
        "--proxy-tls-key",
        "--proxy-public-host",
    ] {
        assert!(run.stdout.contains(flag), "{flag}: {}", run.stdout);
    }
}

#[test]
fn binding_show_prints_the_remote_address_a_wide_host_records() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    {
        let engine = hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap();
        engine
            .blocking()
            .create_binding(hekma_engine::BindingSpec {
                name: "openai".into(),
                prefix: "/openai".into(),
                upstream: "https://api.example.com/v1".into(),
                labels: Vec::new(),
                custodied_key: None,
                identity_headers: None,
            })
            .unwrap();
    }
    // What a host bound beyond loopback records (no host is started: the
    // read is local).
    hekma_host::write_discovery(
        state_dir,
        &hekma_host::Discovery {
            schema_version: hekma_host::DISCOVERY_SCHEMA_VERSION,
            pid: std::process::id(),
            addr: "127.0.0.1:9".parse().unwrap(),
            token: "0".repeat(64),
            hekma_version: "test".into(),
            autostarted: false,
            metering_proxy_url: Some("http://127.0.0.1:8400".into()),
            metering_proxy_public_url: Some("https://proxy.example:8400".into()),
        },
    )
    .unwrap();
    let run = run_hekma_agent(
        &["binding", "show", "openai", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["remote_base_url"], "https://proxy.example:8400/openai");
    let run = run_hekma_agent(&["binding", "show", "openai"], &ctx.project_dir, state_dir);
    assert!(
        run.stdout
            .contains("from other machines https://proxy.example:8400/openai"),
        "{}",
        run.stdout
    );
}
