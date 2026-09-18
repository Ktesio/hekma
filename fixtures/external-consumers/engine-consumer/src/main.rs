//! The embedding quickstart — a complete Ktesio host in one file (story 7-4).
//!
//! This is the copy-paste starting point for embedding the Ktesio engine as a
//! Rust library. It depends on NOTHING but `hemaka-engine` itself — no `kt`,
//! no CLI, no test fixtures, no helper crates — and drives one mini
//! lifecycle: open → register → configure → subscribe → start → observe →
//! stop. The full flow proof (breach, pause, shared assertions) lives in the
//! test suite; this file is the embedder's hello-world.
//!
//! Run it from the repo root with `cargo run -p hemaka-engine --example
//! embedding-quickstart`. The walkthrough lives at docs/embedding.md.
//!
//! ## How it stays hermetic and dependency-free
//!
//! A real host's manifest points `[lifecycle.start]` at its own agent
//! executable. This example has no agent binary, so it is its own agent: it
//! re-executes ITSELF with a sentinel argument, and the agent side of `main`
//! idles until the engine's stop ends the process. The scratch state root is
//! a per-process directory under the OS temp dir, removed best-effort at the
//! end — so the example needs no helper crates at all. One honesty note: the
//! idle cap BOUNDS any orphan to two minutes (it does not prevent one — a
//! crashed host still leaves the agent briefly); the ordinary end is the
//! stop below.
//!
//! Every step asserts with `expect`/`assert_eq!`; a real host should map
//! these into its own error type instead of panicking.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::broadcast;
use engine::{AdapterRef, ConfigLayer, Engine, LifecycleState};

/// The instance name the quickstart registers under.
const INSTANCE: &str = "hello-agent";

/// The sentinel argument that turns a re-execution of this binary into the
/// agent process the engine supervises.
const AGENT_ARG: &str = "--hemaka-rename-fixture-agent";

/// How long the agent side idles if the engine never stops it. This BOUNDS
/// any orphan to two minutes (it does not prevent one); the ordinary end is
/// the stop below.
const AGENT_IDLE_CAP: Duration = Duration::from_secs(120);

/// The token ceiling the configure leg arms, asserted back on read.
const TOKEN_CEILING: &str = "100000";

fn main() {
    // ---- The agent side: what the engine's `start` below spawns. ----
    if std::env::args().any(|arg| arg == AGENT_ARG) {
        std::thread::sleep(AGENT_IDLE_CAP);
        return;
    }

    // ---- The host side: the whole quickstart. ----

    // 1. A scratch state root: a per-process directory under the OS temp dir
    //    (the pid keeps concurrent runs apart), cleaned up best-effort at the
    //    end. A real host passes its own state directory — or `None` for the
    //    OS default.
    let root = std::env::temp_dir().join(format!("hemaka-rename-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root); // a stale run that reused this pid
    std::fs::create_dir_all(&root).expect("create the scratch root");

    // 2. Open the engine on that root. It owns its tokio runtime and holds no
    //    global state — several engines may live in one process, each with
    //    its own root. `blocking()` is the synchronous facade; a host with an
    //    async runtime may call the engine's async methods directly instead.
    let engine = Engine::open(Some(root.clone())).expect("open the engine");
    let host = engine.blocking();

    // 3. Register a manifest adapter (the `adapter.toml` fixture below). A
    //    host can also register a built-in kind, e.g.
    //    `AdapterRef::Native("hermes".into())`.
    let manifest_dir = write_manifest(&root.join("hello-adapter"));
    host.register_with_adapter(INSTANCE, &AdapterRef::Manifest(manifest_dir))
        .expect("register the agent");

    // 4. Configure one budget key and ASSERT the read-back: keys validate at
    //    write time, persist in the Agent Home, and resolve with provenance.
    host.set_config(INSTANCE, "budget.tokens.cumulative", TOKEN_CEILING)
        .expect("set the token budget");
    let effective = host
        .effective_config(INSTANCE, ConfigLayer::empty())
        .expect("read the effective config");
    assert_eq!(
        effective
            .value_display("budget.tokens.cumulative")
            .as_deref(),
        Some(TOKEN_CEILING),
        "the configured ceiling must read back exactly"
    );
    assert_eq!(
        effective.source_label("budget.tokens.cumulative"),
        Some("instance"),
        "the configured key must resolve at the instance layer"
    );

    // 5. Subscribe BEFORE starting: a subscriber observes only events
    //    committed after it subscribed. The bus is bounded; a slow subscriber
    //    observes `Lagged` and resyncs at the tail — supervision is never
    //    stalled by a subscriber.
    let mut events = host.subscribe();

    // 6. Start: the engine spawns the manifest's `[lifecycle.start]` command
    //    and drives the instance to `running` before this returns.
    host.start(INSTANCE).expect("start the agent");

    // 7. Observe: drain what has committed so far — the `starting` and
    //    `running` transitions are certain, so at least one event MUST have
    //    arrived. `Lagged` is NOT the end of the stream: the dropped events
    //    stay readable in the durable logs and the receiver resyncs at the
    //    tail, so keep draining; `Closed`/`Empty` are.
    let mut seen = 0;
    loop {
        match events.try_recv() {
            Ok(event) => {
                println!("event: {event:?}");
                seen += 1;
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                println!("resynced at the tail after lagging {dropped} events");
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break
            }
        }
    }
    assert!(
        seen >= 1,
        "the committed transitions must reach the subscriber"
    );
    let status = host.instance_status(INSTANCE).expect("instance status");
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the agent must be running after start"
    );
    let entry = host
        .fleet()
        .expect("fleet read")
        .into_iter()
        .find(|e| e.name.as_str() == INSTANCE)
        .expect("the agent is in the Fleet");
    assert_eq!(
        entry.budget.as_ref().and_then(|b| b.cumulative_limit),
        Some(TOKEN_CEILING.parse::<u64>().expect("the ceiling parses")),
        "the Fleet row carries the configured ceiling"
    );
    println!(
        "fleet: {} [{}] state={}",
        entry.name.as_str(),
        entry.kind,
        entry.state.as_str(),
    );

    // 8. Stop: graceful shutdown with a 5-second escalation window. The whole
    //    process group/job dies — no orphan survives the host. Then clean up
    //    the scratch root (best-effort: a still-dying handle just leaves it).
    host.stop(INSTANCE, Some(Duration::from_secs(5)))
        .expect("stop the agent");
    let _ = std::fs::remove_dir_all(&root);
    println!("done — the agent ran under governance and stopped cleanly.");
}

/// Write the manifest fixture (`adapter.toml`) the example registers under:
/// the contract-v1 shape, with `[lifecycle.start]` pointing at THIS binary.
/// A real host writes the same shape pointing at its agent executable — see
/// docs/manifest.md for the full schema.
fn write_manifest(dir: &Path) -> PathBuf {
    let exec = std::env::current_exe().expect("resolve this example's binary");
    // A non-UTF8 binary path would be silently munged by to_string_lossy
    // into a launch that never resolves (the TOML wire is UTF-8 regardless)
    // — fail loudly instead (the perf-budgets harness precedent).
    let exec = exec.to_str().expect(
        "this example's binary path is not valid UTF-8; the TOML manifest requires a UTF-8 path",
    );
    // Forward slashes only: a Windows path's backslashes would need TOML
    // escaping inside this basic string, and Windows file APIs accept
    // forward-slash paths, so replace them instead of escaping.
    let exec = exec.replace('\\', "/");
    std::fs::create_dir_all(dir).expect("create the manifest dir");
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "quickstart"

[lifecycle.start]
exec = {exec:?}
args = ["{AGENT_ARG}"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[metering]
source = "self-reported"
"#
    );
    std::fs::write(dir.join("adapter.toml"), body).expect("write adapter.toml");
    dir.to_path_buf()
}
