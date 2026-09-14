//! Integration tests for story-3-4 ENGINE-OBSERVED usage metering (AC-A / AC-B /
//! AC-C + the 3-2/3-3 enforcement-reuse proofs + the no-leak proof), driven
//! end-to-end through the PUBLIC async [`Engine`] + its background reaper cadence
//! (spine AD-2/AD-13).
//!
//! The shape: a `fake_agent` in `--observed-calls` mode reads the `base_url` the
//! engine INJECTED (via the adapter's config-mapping) and makes real
//! OpenAI-compatible POSTs to the engine's LOOPBACK FORWARD LISTENER, which relays
//! them to a LOCAL UPSTREAM STUB (a tiny pure-`std` HTTP server returning a FIXED
//! `usage`) and skims the parsed usage into the durable Usage Ledger — the whole
//! `engine-observed` interception path, not mocked.
//!
//! ## Robust + cross-OS by construction (retro AI-35/37/38)
//!
//! A single in-process `Engine` kept ALIVE for the whole test (like
//! `tests/metering.rs`), so NO cross-lifetime process survival and NO `OsId`-gated
//! skip anywhere — loopback HTTP + committed-STATE polling is identical on Linux,
//! macOS, and Windows. Determinism: the upstream stub returns a FIXED `usage`
//! (`prompt_tokens`/`completion_tokens`), so K observed calls = an EXACT known
//! token total; each test POLLS the committed ledger until the expected observed
//! row count lands (never a wall-clock sleep against a side file).
//!
//! ## No-leak (2-4 rigor)
//!
//! The forward proxy carries the agent's API key upstream. A sentinel key is sent
//! in the forwarded request; the upstream stub CONFIRMS it received it (the relay
//! is faithful), and the test sweeps EVERY ktesio surface — the engine event log,
//! the breach log, the ledger DB, and the agent-output log — asserting the sentinel
//! appears in NONE of them (only the parsed integer token counts ever leave the
//! proxy).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ktesio_engine::{AdapterRef, Engine, LifecycleState};
use tempfile::TempDir;

/// The FIXED token counts the upstream stub returns in every completion's `usage`
/// object — so ledger totals are exact-match assertions (K calls × these).
const STUB_PROMPT_TOKENS: u64 = 30;
const STUB_COMPLETION_TOKENS: u64 = 70;

/// A sentinel API key value the `fake_agent` forwards in the `Authorization`
/// header; the no-leak sweep asserts it appears in NONE of ktesio's surfaces.
const SENTINEL_API_KEY: &str = "sk-ktesio-no-leak-sentinel-3-4-abcdef";

/// A handle to a running local upstream stub (a loopback HTTP server). Dropping it
/// signals the accept loop to stop.
struct UpstreamStub {
    /// The `http://127.0.0.1:<port>` base URL the engine forwards to (set as
    /// `metering.upstream_base_url`).
    base_url: String,
    /// Set once the stub has seen the sentinel API key in a forwarded request
    /// (proves the proxy relays the auth header faithfully upstream).
    saw_sentinel_key: Arc<AtomicBool>,
    /// How many completion requests the stub has served.
    served: Arc<AtomicU64>,
    /// Flips to stop the accept loop on drop.
    stop: Arc<AtomicBool>,
}

impl Drop for UpstreamStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Nudge the accept loop out of its blocking accept with a throwaway connect.
        if let Some(authority) = self.base_url.strip_prefix("http://") {
            let _ = TcpStream::connect(authority);
        }
    }
}

/// Start a LOCAL upstream stub on `127.0.0.1:0` (loopback, OS-picked port) that
/// answers every completion POST with a FIXED OpenAI-compatible body carrying the
/// known `usage`. Pure `std` (a `TcpListener` accept loop on a thread) — NO
/// dependency, NO OS-cfg, identical on every OS. Deterministic: the same `usage`
/// every time, so K calls → an exact token total.
fn start_upstream_stub() -> UpstreamStub {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream stub");
    let addr = listener.local_addr().expect("stub addr");
    assert!(addr.ip().is_loopback(), "stub must be loopback");
    let base_url = format!("http://{addr}");
    let saw_sentinel_key = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let thread_saw = Arc::clone(&saw_sentinel_key);
    let thread_served = Arc::clone(&served);
    let thread_stop = Arc::clone(&stop);
    thread::spawn(move || {
        for stream in listener.incoming() {
            if thread_stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut stream) = stream else { continue };
            serve_one(&mut stream, &thread_saw, &thread_served);
        }
    });

    UpstreamStub {
        base_url,
        saw_sentinel_key,
        served,
        stop,
    }
}

/// Serve ONE upstream request: read the request head (headers) + any body, note
/// whether the sentinel API key was forwarded (faithful relay proof), and write a
/// FIXED OpenAI-compatible completion response with the known `usage`.
fn serve_one(stream: &mut TcpStream, saw_sentinel: &AtomicBool, served: &AtomicU64) {
    // Read the request head (up to the blank line) + drain the declared body length,
    // so the socket is consumed before we respond (a well-behaved HTTP peer).
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    // Read until we have the header terminator (\r\n\r\n) or the peer stalls.
    loop {
        let head_end = find_subsequence(&buf, b"\r\n\r\n");
        if head_end.is_some() {
            break;
        }
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
        if buf.len() > 64 * 1024 {
            break; // defensive cap
        }
    }
    let head = String::from_utf8_lossy(&buf);
    // The faithful-relay proof: the sentinel API key the agent sent must arrive here
    // (the proxy forwarded the Authorization header verbatim upstream).
    if head.contains(SENTINEL_API_KEY) {
        saw_sentinel.store(true, Ordering::SeqCst);
    }

    // A FIXED OpenAI-compatible completion body with the known usage.
    let body = format!(
        r#"{{"id":"chatcmpl-stub","object":"chat.completion","model":"gpt-observed",
"choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],
"usage":{{"prompt_tokens":{STUB_PROMPT_TOKENS},"completion_tokens":{STUB_COMPLETION_TOKENS},"total_tokens":{total}}}}}"#,
        total = STUB_PROMPT_TOKENS + STUB_COMPLETION_TOKENS,
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    served.fetch_add(1, Ordering::SeqCst);
}

/// A tiny substring search (no dependency) — the head-terminator finder.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Write an `engine-observed` `fake_agent` manifest: the `[lifecycle.start]` exec
/// is `fake_agent` + `args`; `[metering] source = "engine-observed"`; and the
/// `[config."metering.base_url"]` mapping points at env `OPENAI_BASE_URL` (so the
/// engine injects the loopback listener URL there — AC6). The operator sets the
/// real upstream via `metering.upstream_base_url` config (not the manifest).
/// `config_section` appends extra TOML after the declared mappings (the
/// `memory.rs` fixture shape) — e.g. a mapping for the reserved `memory.dir` key.
fn write_observed_manifest(dir: &Path, kind: &str, args: &[&str], config_section: Option<&str>) {
    let bin = ktesio_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "{kind}"

[lifecycle.start]
exec = {exec:?}
args = [{args_toml}]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "engine-observed"

[config.model]
env = "MODEL"

[config."metering.base_url"]
env = "OPENAI_BASE_URL"
{config_section}"#,
        exec = bin.to_string_lossy(),
        config_section = config_section.unwrap_or(""),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// The number of `usage_events` rows for `name` with a given `metering_source`,
/// read via a direct read-only connection to the state DB (committed STATE).
fn observed_row_count(state_dir: &Path, name: &str) -> u64 {
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).expect("open state db");
    conn.query_row(
        "SELECT COUNT(*) FROM usage_events e \
         JOIN agent_instances i ON i.id = e.instance_id \
         WHERE i.name = ?1 AND e.metering_source = 'engine-observed'",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

/// Poll the committed engine-observed row count until it reaches `expected`, bounded.
fn wait_for_observed_rows(state_dir: &Path, name: &str, expected: u64, within: Duration) -> u64 {
    let deadline = Instant::now() + within;
    loop {
        let count = observed_row_count(state_dir, name);
        if count >= expected {
            return count;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} engine-observed rows for '{name}' (have {count})"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Poll the committed lifecycle STATE until it reaches `target`, bounded.
fn wait_for_state(engine: &Engine, name: &str, target: LifecycleState, within: Duration) {
    let facade = engine.blocking();
    let deadline = Instant::now() + within;
    loop {
        if let Ok(status) = facade.instance_status(name) {
            if status.instance.state == target {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for '{name}' to reach {target:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn engine_observed_usage_lands_in_the_ledger_tagged_engine_observed() {
    // AC-A: an `engine-observed` agent's model traffic is intercepted by the loopback
    // forward listener, its `usage` parsed and landed in the ledger under the Run id,
    // tagged `engine-observed`; the Fleet-detail totals equal the ledger exactly.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let stub = start_upstream_stub();

    // The agent makes 3 observed calls to its injected base_url, then lingers.
    write_observed_manifest(
        manifest.path(),
        "obs",
        &["--observed-calls", "3", "--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("obs", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    // The operator configures the REAL upstream (the stub) — the engine forwards there.
    facade
        .set_config("obs", "metering.upstream_base_url", &stub.base_url)
        .unwrap();

    let started = facade.start("obs").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Wait for all 3 OBSERVED events to commit (the reaper drains the listener queue).
    let count = wait_for_observed_rows(state.path(), "obs", 3, Duration::from_secs(30));
    assert_eq!(
        count, 3,
        "exactly 3 engine-observed rows (one per observed call)"
    );

    // The stub actually served the forwarded calls (the relay reached upstream).
    assert!(
        stub.served.load(Ordering::SeqCst) >= 3,
        "the upstream stub served the forwarded calls"
    );

    // The Fleet-detail totals EQUAL the ledger exactly (FR-22): 3 × (30 in, 70 out).
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "obs").unwrap();
    assert_eq!(entry.usage.cumulative_input_tokens, 3 * STUB_PROMPT_TOKENS);
    assert_eq!(
        entry.usage.cumulative_output_tokens,
        3 * STUB_COMPLETION_TOKENS
    );
    // The active Metering Source is visible in Fleet detail as `engine-observed` (the
    // 3-1 AC-C visibility now showing the SECOND source).
    assert_eq!(entry.metering_source, "engine-observed");

    let _ = facade.stop("obs", Some(Duration::from_secs(5)));
}

#[test]
fn a_token_budget_enforces_on_observed_usage_and_pauses() {
    // Task 6 (3-2 reuse proof): a token budget crosses on OBSERVED usage → the SAME
    // enforcement path pauses the instance (default action), with NO new code. Each
    // observed call is 100 tokens (30+70); a cumulative budget of 150 is crossed by
    // the 2nd observed event.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let stub = start_upstream_stub();
    write_observed_manifest(
        manifest.path(),
        "obsbudget",
        &["--observed-calls", "5", "--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "obsbudget",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config("obsbudget", "metering.upstream_base_url", &stub.base_url)
        .unwrap();
    // A cumulative token ceiling the observed consumption crosses (100 tokens/call).
    facade
        .set_config("obsbudget", "budget.tokens.cumulative", "150")
        .unwrap();

    facade.start("obsbudget").unwrap();

    // The instance PAUSES on the breaching observed event (the enforcement is
    // synchronous in ingest_usage). Poll the committed lifecycle state.
    wait_for_state(
        &engine,
        "obsbudget",
        LifecycleState::Paused,
        Duration::from_secs(30),
    );

    // A token breach was recorded (FR-21 always-recorded), carrying the
    // `engine-observed` source — proving enforcement fired on observed usage.
    let breaches = facade.budget_breach_events("obsbudget").unwrap();
    assert!(
        !breaches.is_empty(),
        "a token breach must be recorded on observed usage"
    );
    assert!(
        breaches
            .iter()
            .any(|b| b.metering_source == "engine-observed"),
        "the breach carries the engine-observed source: {breaches:?}"
    );

    let _ = facade.stop("obsbudget", Some(Duration::from_secs(5)));
}

#[test]
fn a_dollar_cap_enforces_on_observed_usage_labeled_estimated() {
    // Task 6 (3-3 reuse proof): a dollar cap crosses on OBSERVED usage → the SAME
    // enforcement pauses, and the derived cost carries EstimateLabel::Estimated (an
    // observed count is an ESTIMATE, never presented as a self-reported actual). With
    // a Rate of $1/1M in + $1/1M out, each 100-token call ≈ $0.0001; a cumulative cap
    // of $0.00015 is crossed by the 2nd observed event.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let stub = start_upstream_stub();
    write_observed_manifest(
        manifest.path(),
        "obsdollar",
        &["--observed-calls", "5", "--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "obsdollar",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config("obsdollar", "metering.upstream_base_url", &stub.base_url)
        .unwrap();
    // A Rate (both directions required) + a cumulative dollar cap the observed cost
    // crosses. $1.00 per 1M tokens each direction.
    facade
        .set_config("obsdollar", "cost.rate.input", "1.00")
        .unwrap();
    facade
        .set_config("obsdollar", "cost.rate.output", "1.00")
        .unwrap();
    // 100 tokens/call at $1/1M ≈ $0.0001/call; cap $0.00015 → crossed by call 2.
    facade
        .set_config("obsdollar", "budget.dollars.cumulative", "0.00015")
        .unwrap();

    facade.start("obsdollar").unwrap();

    wait_for_state(
        &engine,
        "obsdollar",
        LifecycleState::Paused,
        Duration::from_secs(30),
    );

    // A DOLLAR breach was recorded, labeled `estimated` (an observed figure is an
    // estimate — 3-3's label rides unchanged), carrying the `engine-observed` source.
    let breaches = facade.budget_breach_events("obsdollar").unwrap();
    let dollar = breaches
        .iter()
        .find(|b| b.dollar_limit.is_some())
        .expect("a dollar breach must be recorded on observed usage");
    assert_eq!(
        dollar.metering_source, "engine-observed",
        "the dollar breach carries the engine-observed source"
    );
    assert_eq!(
        dollar.estimate_label.map(|l| l.as_str()),
        Some("estimated"),
        "an observed dollar figure is labeled estimated, never a self-reported actual"
    );

    let _ = facade.stop("obsdollar", Some(Duration::from_secs(5)));
}

#[test]
fn the_forwarded_api_key_never_leaks_into_any_ktesio_surface() {
    // No-leak (2-4 rigor): the proxy carries the agent's API key upstream FAITHFULLY,
    // but the key must appear in NONE of ktesio's surfaces — the engine event log, the
    // breach log, the ledger DB, or the agent-output log. The `fake_agent` forwards a
    // SENTINEL key; the upstream stub confirms it received it (faithful relay), then we
    // sweep every surface for the sentinel and assert it is absent.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let stub = start_upstream_stub();
    // The agent forwards the sentinel key on its requests — but the key is NOT a
    // manifest-arg literal (that would land in the persisted registration
    // snapshot). It is delivered as a `secret:` config leaf below, so it reaches
    // the child ONLY in its start-time environment and the sweep can cover the
    // whole Agent Home (adapter.json included).
    write_observed_manifest(
        manifest.path(),
        "noleak",
        &["--observed-calls", "2", "--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "noleak",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config("noleak", "metering.upstream_base_url", &stub.base_url)
        .unwrap();

    // Deliver the sentinel as a SECRET via a pass-through env leaf (mirrors
    // agent_cli.rs's `secret:` env-target delivery): the reference is stored in
    // config, the cleartext is resolved at START from the env resolver and placed
    // into the child env var `FAKE_AGENT_OBSERVED_AUTH` (which the fake_agent reads
    // for its forwarded Authorization header) — NEVER into the registration
    // snapshot. A UNIQUE secret-name + restore avoids racing sibling in-process
    // tests (the established `set_var` pattern).
    let secret_env = "KTESIO_NOLEAK_SENTINEL_OK";
    let prev = std::env::var_os(secret_env);
    std::env::set_var(secret_env, SENTINEL_API_KEY);
    facade
        .set_config(
            "noleak",
            "agent.FAKE_AGENT_OBSERVED_AUTH",
            &format!("secret:{secret_env}"),
        )
        .unwrap();

    facade.start("noleak").unwrap();
    wait_for_observed_rows(state.path(), "noleak", 2, Duration::from_secs(30));

    // The proxy relayed the key faithfully UPSTREAM (the stub saw it) — so the key DID
    // flow through the proxy; the point is it does not leak into ktesio's own surfaces.
    assert!(
        stub.saw_sentinel_key.load(Ordering::SeqCst),
        "the proxy must forward the Authorization header faithfully upstream"
    );

    // Sweep EVERY ktesio surface for the sentinel key — it must appear in NONE.
    // (1) the ledger DB (the whole file, as bytes).
    let db_bytes = std::fs::read(state.path().join("state.db")).unwrap_or_default();
    assert!(
        !contains(&db_bytes, SENTINEL_API_KEY.as_bytes()),
        "the API key must NOT appear in the ledger DB"
    );
    // (2) the WHOLE Agent Home recursively — the engine transition-event log, the
    // breach log, the agent-output log, AND the registration snapshot adapter.json
    // (no exclusions: the secret-leaf delivery keeps the sentinel out of every one).
    let leaked = sweep_dir_for(&state.path().join("agents"), SENTINEL_API_KEY);
    assert!(
        leaked.is_none(),
        "the API key leaked into a ktesio file: {:?}",
        leaked
    );
    // (5) the transition events surfaced through the public API.
    let events = facade.transition_events("noleak").unwrap();
    let events_json = serde_json::to_string(&events).unwrap();
    assert!(
        !events_json.contains(SENTINEL_API_KEY),
        "the API key leaked into a transition event"
    );

    let _ = facade.stop("noleak", Some(Duration::from_secs(5)));
    // Restore the env the unique secret-name borrowed.
    match prev {
        Some(v) => std::env::set_var(secret_env, v),
        None => std::env::remove_var(secret_env),
    }
}

#[test]
fn the_forwarded_api_key_never_leaks_on_the_forward_failure_path() {
    // No-leak, ERROR PATH (2-4 rigor, L2): the happy-path sweep above proves the key
    // does not leak when the relay SUCCEEDS. This proves the same across a forward
    // FAILURE: the upstream is UNREACHABLE (a set-but-dead port), so every forwarded
    // request — carrying the sentinel Authorization key — fails as an honest 502. We
    // then sweep every error surface (the ledger DB, the per-instance logs: agent
    // output + breach + transition-event files, and the transition events via the
    // public API) and assert the sentinel key is ABSENT on the error path too.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();

    // A set-but-DEAD upstream: bind an ephemeral loopback port, capture it, drop the
    // listener so nothing is listening. The instance STARTS (the upstream URL is
    // present + valid http://), but every forward gets connection-refused → 502.
    let dead_upstream = {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = l.local_addr().unwrap();
        format!("http://{addr}")
        // listener dropped here → the port is dead
    };

    // The agent makes several observed calls carrying the sentinel key; each fails at
    // the proxy (dead upstream). No `usage` is ever parsed (nothing lands in the
    // ledger), but the KEY still flowed through the failing forward. The key is
    // delivered as a `secret:` config leaf below (NOT a manifest-arg literal), so it
    // stays out of the registration snapshot and the sweep can cover the whole home.
    write_observed_manifest(
        manifest.path(),
        "noleakerr",
        &["--observed-calls", "3", "--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "noleakerr",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config("noleakerr", "metering.upstream_base_url", &dead_upstream)
        .unwrap();

    // Deliver the sentinel as a SECRET via a pass-through env leaf (see the
    // happy-path test): resolved at START into the child env, never the snapshot. A
    // UNIQUE secret-name (distinct from the happy-path test) + restore avoids racing
    // the sibling in-process test.
    let secret_env = "KTESIO_NOLEAK_SENTINEL_ERR";
    let prev = std::env::var_os(secret_env);
    std::env::set_var(secret_env, SENTINEL_API_KEY);
    facade
        .set_config(
            "noleakerr",
            "agent.FAKE_AGENT_OBSERVED_AUTH",
            &format!("secret:{secret_env}"),
        )
        .unwrap();

    let started = facade.start("noleakerr").unwrap();
    assert_eq!(
        started.state,
        LifecycleState::Running,
        "a set-but-dead upstream still starts (the failure is per-forward, not at start)"
    );

    // Give the agent time to make (and fail) its forwards through the proxy, so the
    // error path is genuinely exercised before we sweep. The forwards fail fast
    // (connection-refused), and the fake_agent lingers, so poll a bounded settle.
    thread::sleep(Duration::from_millis(1500));

    // No observed rows ever landed (every forward failed) — the ledger stays empty of
    // engine-observed usage for this instance. (A sanity check that the path we swept
    // really was the FAILURE path, not an accidental success.)
    assert_eq!(
        observed_row_count(state.path(), "noleakerr"),
        0,
        "a dead upstream yields NO parsed usage (the forward-failure path)"
    );

    // Sweep EVERY error surface for the sentinel key — it must appear in NONE.
    // (1) the ledger DB (the whole file, as bytes).
    let db_bytes = std::fs::read(state.path().join("state.db")).unwrap_or_default();
    assert!(
        !contains(&db_bytes, SENTINEL_API_KEY.as_bytes()),
        "the API key must NOT appear in the ledger DB on the error path"
    );
    // (2) the WHOLE Agent Home recursively — agent-output + breach + transition-event
    // files AND the registration snapshot adapter.json (no exclusions).
    let leaked = sweep_dir_for(&state.path().join("agents"), SENTINEL_API_KEY);
    assert!(
        leaked.is_none(),
        "the API key leaked into a ktesio file on the error path: {:?}",
        leaked
    );
    // (3) the transition events surfaced through the public API.
    let events = facade.transition_events("noleakerr").unwrap();
    let events_json = serde_json::to_string(&events).unwrap();
    assert!(
        !events_json.contains(SENTINEL_API_KEY),
        "the API key leaked into a transition event on the error path"
    );

    let _ = facade.stop("noleakerr", Some(Duration::from_secs(5)));
    // Restore the env the unique secret-name borrowed.
    match prev {
        Some(v) => std::env::set_var(secret_env, v),
        None => std::env::remove_var(secret_env),
    }
}

#[test]
fn an_engine_observed_instance_with_no_upstream_fails_to_start_cleanly() {
    // AC-A companion: an `engine-observed` instance with NO configured upstream URL
    // fails the start with a clear error and NO state change (the listener has nowhere
    // to forward). It never reaches `running`.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_observed_manifest(
        manifest.path(),
        "noup",
        &["--observed-calls", "1", "--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("noup", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    // No `metering.upstream_base_url` set → the start must reject.
    let err = facade.start("noup").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("metering.upstream_base_url") || msg.contains("engine-observed"),
        "the error names the missing upstream config: {msg}"
    );
    // The instance stayed out of `running` (prior state preserved).
    let status = facade.instance_status("noup").unwrap();
    assert_ne!(
        status.instance.state,
        LifecycleState::Running,
        "a failed engine-observed start must not reach running"
    );
}

#[test]
fn a_hand_set_memory_dir_never_reaches_an_engine_observed_start_surfaces() {
    // Story 11-3 (A1): an engine-observed start takes the INVOCATION-OVERRIDE
    // branch (the listener's base_url forces the config re-fold), and that
    // re-fold re-derives the config from EVERY layer. A hand-set reserved
    // `memory.dir` must NOT resurrect through it: with no backing attached the
    // engine injects nothing at this key, so the only way the decoy could
    // reach the agent is by leaking through the re-fold — it must appear in
    // NEITHER the delivered env (the child's dump) NOR the persisted
    // effective-config snapshot (the memory.rs honest-provenance pattern).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();

    // The dump target lives OUTSIDE the Agent Home so the home stays pristine
    // for the snapshot assertion below.
    let dump = state.path().join("memory-strip.dump");
    let dump_arg = format!("{}", dump.display());
    // The observed manifest DECLARES a mapping for the reserved key (env
    // `AGENT_MEMORY_DIR` — the memory.rs fixture spelling, deliberately not an
    // engine constant), so an unstripped value WOULD be delivered.
    write_observed_manifest(
        manifest.path(),
        "obsmem",
        &["--linger-ms", "600000", "--dump", &dump_arg],
        Some("\n[config.\"memory.dir\"]\nenv = \"AGENT_MEMORY_DIR\"\n"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    let registered = facade
        .register_with_adapter(
            "obsmem",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    // A valid-shaped (dead) upstream: the listener needs a URL to start; this
    // agent makes no calls (no `--observed-calls`), so nothing ever forwards.
    let dead_upstream = {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = l.local_addr().unwrap();
        format!("http://{addr}")
    };
    facade
        .set_config("obsmem", "metering.upstream_base_url", &dead_upstream)
        .unwrap();
    // THE DECOY: a hand-set value at the reserved key in the operator
    // (instance) layer — the tests/cost.rs set_config pattern.
    let decoy = "/tmp/ktesio-decoy-memory-dir-11-3";
    facade.set_config("obsmem", "memory.dir", decoy).unwrap();
    // POSITIVE CONTROL (review loop 1): an `agent.*` pass-through key set
    // through the SAME operator layer and delivered by the SAME start — its
    // delivery proves the (re-folded) config path is live end-to-end, so the
    // decoy's absence below is attributable to the STRIP, not to a dead
    // mapping or an operator layer that never reaches the child.
    facade
        .set_config("obsmem", "agent.PROBE_CONTROL_KEY", "control-value-11-3")
        .unwrap();

    let started = facade.start("obsmem").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Poll the committed dump (the memory.rs pattern), then assert the decoy
    // reached NOTHING: a stripped key is a mapping no-op, so there is no
    // AGENT_MEMORY_DIR env line at all.
    let deadline = Instant::now() + Duration::from_secs(10);
    let dump_text = loop {
        match std::fs::read_to_string(&dump) {
            Ok(text) if text.contains("arg=") => break text,
            _ => {
                assert!(
                    Instant::now() < deadline,
                    "the agent never wrote its dump at {}",
                    dump.display()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    assert!(
        !dump_text.contains("env=AGENT_MEMORY_DIR="),
        "a hand-set reserved key must not be delivered as engine memory:\n{dump_text}"
    );
    assert!(
        !dump_text.contains(decoy),
        "the decoy must not reach the child's environment anywhere:\n{dump_text}"
    );
    assert!(
        dump_text.contains("env=PROBE_CONTROL_KEY=control-value-11-3"),
        "the positive control must be delivered by the same start (proves the \
         mapping path is live and the strip is what blocked the reserved key):\n{dump_text}"
    );

    // ... and neither the key nor the value lands in the effective-config
    // snapshot (the 3-4/5-1 honest-provenance split).
    let snapshot =
        std::fs::read_to_string(Path::new(&registered.agent_home).join("effective-config.json"))
            .expect("snapshot written at start");
    assert!(
        !snapshot.contains("memory.dir") && !snapshot.contains(decoy),
        "the reserved key must stay out of effective-config.json:\n{snapshot}"
    );

    facade.stop("obsmem", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn adding_engine_observed_did_not_add_a_no_metering_escape_hatch() {
    // AC-C (regression guard, 3-4's obligation): the FR-19 hard line still holds —
    // a manifest with NO `[metering]` section is REJECTED at registration with the
    // section-naming diagnostic. Adding the engine-observed INGESTION path must NOT
    // weaken this. (The 1-3 assertion owns the primary guard; this proves 3-4 did not
    // regress it, AND that `engine-observed` is itself a VALID declaration under
    // contract v1 — story 6-6 froze it at 1.0.0 with `engine-observed` in the
    // vocabulary.)
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();

    // (1) A manifest with NO [metering] section is still rejected (the hard line).
    let bin = ktesio_conformance::fake_agent_bin();
    let no_metering = format!(
        "contract_version = \"1.0.0\"\n[adapter]\nkind = \"nomet\"\n\
         [lifecycle.start]\nexec = {exec:?}\n\
         [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n",
        exec = bin.to_string_lossy(),
    );
    std::fs::write(manifest.path().join("adapter.toml"), no_metering).unwrap();
    let err = facade
        .register_with_adapter(
            "nomet",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .expect_err("a manifest with no [metering] section must be rejected");
    assert!(
        err.to_string().contains("[metering]") || err.to_string().contains("Metering"),
        "the rejection names the missing metering section: {err}"
    );

    // (2) `engine-observed` IS a valid declaration under contract v1 — a
    // manifest declaring it REGISTERS under the frozen 1.0.0 contract.
    let observed_manifest = TempDir::new().unwrap();
    write_observed_manifest(
        observed_manifest.path(),
        "obsok",
        &["--linger-ms", "1000"],
        None,
    );
    facade
        .register_with_adapter(
            "obsok",
            &AdapterRef::Manifest(observed_manifest.path().to_path_buf()),
        )
        .expect("an engine-observed manifest registers under CONTRACT_VERSION 1.0.0");
}

/// Recursively read EVERY file under `dir` and return the first path whose bytes
/// contain `needle`, or `None` if the needle appears nowhere (the no-leak sweep).
///
/// No exclusions — the whole Agent Home is swept, INCLUDING the registration
/// snapshot `adapter.json`. The registration snapshot persists the manifest's
/// `[lifecycle.start]` launch (exec + args + env), so a sentinel hard-coded as a
/// manifest arg WOULD land here; these tests therefore deliver the sentinel as a
/// `secret:` config leaf resolved into the child env at START (never a manifest
/// literal, never the snapshot), keeping the sweep whole so any future leak into
/// `adapter.json` is a true positive again.
fn sweep_dir_for(dir: &Path, needle: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = sweep_dir_for(&path, needle) {
                return Some(found);
            }
        } else if let Ok(bytes) = std::fs::read(&path) {
            if contains(&bytes, needle.as_bytes()) {
                return Some(path);
            }
        }
    }
    None
}

/// Byte-substring containment (the no-leak sweep works on raw bytes so a binary DB
/// or a log with odd encoding is still swept honestly).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find_subsequence(haystack, needle).is_some()
}
