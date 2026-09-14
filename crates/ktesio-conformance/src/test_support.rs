//! The ONE test-support home for the embedding suites (story 10-1, issue
//! #164): the parameterized manifest-TOML fixture builder with named presets,
//! the lag-accumulating subscription drains for BOTH receiver forms, and the
//! `fake_agent` locator parameterized over the cargo target subdirectory the
//! running binary resolves from.
//!
//! ## Role: test infrastructure, never a driving surface
//!
//! Epic 7's embedding suites each grew near-identical private copies of the
//! same three helpers — a manifest builder, a `try_recv` drain, a
//! `fake_agent` resolver — so a schema or convention change had to be
//! replicated by hand in every copy (the exact failure mode #164 predicted).
//! This module is the consolidation: a fixture shape is stated ONCE here,
//! and the suites story 10-1 enumerated consume it — `uj3::
//! write_flow_manifest`'s callers (the 7-1/7-3 suites), the
//! event-subscription suite's lingering/crash-once/replay fixtures, the
//! perf-budgets heartbeat fixture, and `kt`'s `agent_cli.rs` fake_agent
//! shapes; since stories 10-2/10-3 the diagnostic-sink and resync suites do
//! too. NOT every suite consumes it: the lifecycle/metering/budget-family
//! engine suites keep their own private builders, deliberately outside the
//! consolidation's enumerated scope — docs/testing.md's consumer inventory
//! is the accurate statement. A test imports these helpers
//! to BUILD fixtures and OBSERVE streams; the driving happens exclusively
//! through each suite's own sanctioned surface (the engine's public
//! `Blocking` facade, documented `kt` commands). This module is deliberately
//! NOT a driver and offers no way to become one.
//!
//! Both consumers dev-depend on this crate; dev-deps never cross the shipping
//! boundary gate (`cargo tree -p ktesio -e normal,build` stays clean).
//!
//! ## What lives here vs. where it came from
//!
//! * [`ManifestFixture`] — the ONE parameterized builder (kind, start args,
//!   per-OS capabilities, metering source, optional `[config.*]` env
//!   mappings) plus named presets for the known shapes
//!   ([`ManifestFixture::uj3_flow`] — the primary preset, delegating from
//!   `uj3::write_flow_manifest` — plus `lingering`, `fake_agent`,
//!   `crash_once`, `replay_batch`, `heartbeat`).
//! * [`drain_raw_receiver`] / [`drain_subscription`] — ONE lag-accumulating
//!   `try_recv` drain ([`drain_with`]) covering both receiver forms,
//!   differing ONLY in the `Closed` policy; `uj3::drain_receiver` delegates.
//! * [`fake_agent_bin`] (re-exported at the crate root) /
//!   [`fake_agent_bin_in`] — the ONE locator, parameterized over [`BinDir`]
//!   so a test binary's `deps/` hop and an example's `examples/` hop share
//!   the resolution + on-demand-build logic.
//!
//! Two fixtures stay deliberately standalone: the embedding quickstart's
//! inline manifest (the host copy-paste artifact — its independence is the
//! point) and `agent_cli.rs`'s raw-body writer (it must produce
//! intentionally INVALID manifests for the failure-path tests).

use std::path::{Path, PathBuf};

use ktesio_adapter_api::MeteringSource;
use ktesio_engine::{broadcast, EngineEvent, EventSubscription};

// ---------------------------------------------------------------------------
// The parameterized manifest-TOML fixture builder
// ---------------------------------------------------------------------------

/// The ONE parameterized `adapter.toml` fixture builder: the general shape
/// (contract v1, `[adapter]` kind, `[lifecycle.start]` exec + args, per-OS
/// `[capabilities.*]`, `[metering]` source, optional `[config.<key>]` env
/// mappings) with named preset constructors for the shapes the suites
/// actually pin. Consume a preset (or the [`ManifestFixture::new`] chain)
/// and finish with [`ManifestFixture::write`] — call sites are one line, and
/// a manifest-schema or convention change edits exactly ONE place: here.
///
/// Serialization rules shared by every shape (stated once):
///
/// * `contract_version` is the frozen Adapter Contract
///   ([`ktesio_adapter_api::CONTRACT_VERSION`]) — never a per-suite literal.
/// * `exec` defaults to the conformance `fake_agent`
///   ([`fake_agent_bin`]); override with [`ManifestFixture::exec`] where the
///   caller resolves from a different hop. A non-UTF-8 exec path is a LOUD
///   panic (the TOML wire is UTF-8; a lossy munge would silently produce a
///   launch that never resolves — the uj3/perf-budgets precedent).
/// * exec path separators are normalized to forward slashes (the
///   perf-budgets heartbeat precedent): a no-op on Unix and a byte-stable,
///   spawn-equivalent form on Windows, so the TOML exec is identical on all
///   three OSes.
/// * args are TOML Debug-quoted strings joined with `", "` — the shape every
///   historical builder emitted and the uj3 shape assertions pin.
pub struct ManifestFixture {
    kind: String,
    args: Vec<String>,
    exec: Option<PathBuf>,
    capabilities: Vec<CapabilityBlock>,
    metering_source: String,
    config_env: Vec<(String, String)>,
}

/// One `[capabilities.<name>]` block: the per-OS `os = "level"` entries in
/// declaration order.
struct CapabilityBlock {
    name: String,
    entries: Vec<(String, String)>,
}

impl ManifestFixture {
    /// The general constructor: adapter `kind` plus the `[lifecycle.start]`
    /// `args` (the fake_agent CLI). Everything else starts at the common
    /// defaults — exec = [`fake_agent_bin`], no capabilities, metering =
    /// `self-reported` (the viable source every fixture registers under),
    /// no config mappings — and is shaped by the chain methods.
    pub fn new(kind: &str, args: &[&str]) -> Self {
        Self {
            kind: kind.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            exec: None,
            capabilities: Vec::new(),
            metering_source: MeteringSource::SelfReported.as_str().to_string(),
            config_env: Vec::new(),
        }
    }

    // -- Named presets (the known shapes; call sites shrink to one line) --

    /// The PRIMARY preset: the UJ-3 flow fixture (story 7-1) — the exact
    /// shape `uj3::write_flow_manifest` pins and documents (contract v1,
    /// `--emit-usage 5 --linger-ms 600000`, pause + interaction guaranteed on
    /// all three OSes so the default pause Breach Action lands `paused`
    /// deterministically everywhere, self-reported metering, and the
    /// `[config.model]` env mapping that makes the configure leg meaningful).
    /// The metering source comes from `uj3::METERING_SOURCE` — the constant
    /// is THE fixture's declaration (not this builder's default), so the
    /// constant and the wire cannot drift apart silently (the cross-check
    /// test below pins the two homes equal too).
    pub fn uj3_flow() -> Self {
        Self::new(
            crate::uj3::MANIFEST_KIND,
            &[
                "--emit-usage",
                &crate::uj3::EMIT_EVENTS.to_string(),
                "--linger-ms",
                "600000",
            ],
        )
        .guaranteed_on_all_oses("interaction")
        .guaranteed_on_all_oses("pause")
        .metering(crate::uj3::METERING_SOURCE)
        .config_env(crate::uj3::MODEL_KEY, crate::uj3::MODEL_ENV_VAR)
    }

    /// Preset: a LINGERING agent (no usage emission) with pause + interaction
    /// `guaranteed` on all three OSes — the cross-OS pause shape the
    /// slow-subscriber and FIFO families hammer.
    pub fn lingering(kind: &str) -> Self {
        Self::new(kind, &["--linger-ms", "600000"])
            .guaranteed_on_all_oses("interaction")
            .guaranteed_on_all_oses("pause")
    }

    /// Preset: a `fake_agent` with arbitrary args, interaction guaranteed on
    /// all three OSes (the readiness line is standard), NO pause declaration
    /// — the `crash.rs`/`metering.rs` generic shape.
    pub fn fake_agent(kind: &str, args: &[&str]) -> Self {
        Self::new(kind, args).guaranteed_on_all_oses("interaction")
    }

    /// Preset: an agent that crashes ONCE after its first launch, then
    /// lingers — the proven `tests/crash.rs` `--crash-times` pattern, with
    /// the cross-restart counter at `crash_state`.
    ///
    /// The crash delay COMFORTABLY EXCEEDS the engine's readiness window
    /// (READINESS_WINDOW = 300ms — the same rule supervisor.rs's own tests
    /// state) or the crash can land inside `watch_startup`, which records a
    /// starting→failed LAUNCH failure — a path that never consults the
    /// restart policy — and the start would error instead of later
    /// crash-detecting. 1500ms is 5× the window: past it on a cold, loaded
    /// first run.
    pub fn crash_once(kind: &str, crash_state: &Path) -> Self {
        Self::new(
            kind,
            &[
                "--crash-after-ms",
                "1500",
                "--crash-times",
                "1",
                "--crash-state",
                &*crash_state.to_string_lossy(),
            ],
        )
        .guaranteed_on_all_oses("interaction")
    }

    /// Preset: a usage emitter that re-emits sequence 0 after its
    /// `emit_events`-event batch (`--replay-usage`) — the `metering.rs`
    /// replay pattern the 7-2 VG2 silence test subscribes to.
    pub fn replay_batch(kind: &str, emit_events: u64) -> Self {
        Self::new(
            kind,
            &[
                "--emit-usage",
                &emit_events.to_string(),
                "--replay-usage",
                "--linger-ms",
                "600000",
            ],
        )
        .guaranteed_on_all_oses("interaction")
    }

    /// Preset: the heartbeat-only idler the perf-budgets harness measures
    /// (story 7-5) — `--heartbeat-ms` only (no usage emission, so the
    /// steady-state window measures supervision, not metering ingestion), a
    /// self-exit bound capping any orphan, pause + interaction guaranteed ×3,
    /// and the `[config.model]` env mapping. Exec: resolve from the
    /// `examples/` hop at the call site ([`ManifestFixture::exec`] +
    /// [`BinDir::Examples`]).
    pub fn heartbeat(kind: &str, heartbeat_ms: u64, linger_ms: u64) -> Self {
        Self::new(
            kind,
            &[
                "--heartbeat-ms",
                &heartbeat_ms.to_string(),
                "--linger-ms",
                &linger_ms.to_string(),
            ],
        )
        .guaranteed_on_all_oses("interaction")
        .guaranteed_on_all_oses("pause")
        .config_env("model", "MODEL")
    }

    // -- Chain methods (the parameterization; also the kt single-OS shapes) --

    /// Override the `[lifecycle.start]` exec (the default is the conformance
    /// `fake_agent`). The perf-budgets example passes the [`BinDir::Examples`]
    /// resolution here — an example's `current_exe` lands in
    /// `target/<profile>/examples/`, so the default deps-hop resolution would
    /// look one directory too deep.
    pub fn exec(mut self, exec: impl Into<PathBuf>) -> Self {
        self.exec = Some(exec.into());
        self
    }

    /// `capability = "guaranteed"` for ALL three modeled OSes — the
    /// cross-OS-deterministic shape (the uj3 flow's pause determinism).
    pub fn guaranteed_on_all_oses(mut self, capability: &str) -> Self {
        for os in ["linux", "macos", "windows"] {
            self = self.capability_on_os(capability, os, "guaranteed");
        }
        self
    }

    /// A single-OS `os = "level"` entry under `[capabilities.<capability>]`.
    /// Entries for the same capability MERGE into one block in declaration
    /// order; the current-OS projection shapes
    /// (`kt`'s pause/interaction level fixtures) build on this.
    pub fn capability_on_os(mut self, capability: &str, os: &str, level: &str) -> Self {
        if let Some(block) = self
            .capabilities
            .iter_mut()
            .find(|block| block.name == capability)
        {
            block.entries.push((os.to_string(), level.to_string()));
        } else {
            self.capabilities.push(CapabilityBlock {
                name: capability.to_string(),
                entries: vec![(os.to_string(), level.to_string())],
            });
        }
        self
    }

    /// Override the `[metering]` source wire string (the default is
    /// `self-reported` — the viable source every fixture registers under).
    pub fn metering(mut self, source: &str) -> Self {
        self.metering_source = source.to_string();
        self
    }

    /// A `[config.<key>]` env mapping — what makes a configure leg's set +
    /// read meaningful for the fixture (a mapping exists to deliver through).
    pub fn config_env(mut self, key: &str, env: &str) -> Self {
        self.config_env.push((key.to_string(), env.to_string()));
        self
    }

    /// Write `adapter.toml` into `dir` (created if missing) and return the
    /// directory path — both `AdapterRef::Manifest` and
    /// `kt agent register --manifest` accept a directory.
    pub fn write(self, dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| {
            panic!(
                "could not create the fixture manifest dir {}: {e}",
                dir.display()
            )
        });
        let exec = self.exec.unwrap_or_else(fake_agent_bin);
        // A non-UTF8 exec path would be silently munged by to_string_lossy into
        // a launch that never resolves (the TOML wire is UTF-8 regardless) —
        // fail loudly instead (the perf-budgets harness precedent).
        let exec = exec.to_str().unwrap_or_else(|| {
            panic!(
                "the fake_agent path {} is not valid UTF-8; the TOML manifest requires a UTF-8 path",
                exec.display()
            )
        });
        // Forward-slash normalization (see the struct docs): a no-op on Unix,
        // spawn-equivalent on Windows — one exec form on all three OSes.
        let exec = exec.replace('\\', "/");
        let args_toml = self
            .args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let capabilities = self
            .capabilities
            .iter()
            .map(|block| {
                let entries = block
                    .entries
                    .iter()
                    .map(|(os, level)| format!("{os} = \"{level}\""))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("\n[capabilities.{}]\n{}", block.name, entries)
            })
            .collect::<String>();
        let config = self
            .config_env
            .iter()
            .map(|(key, env)| format!("\n[config.{key}]\nenv = \"{env}\""))
            .collect::<String>();
        let body = format!(
            "\ncontract_version = \"{contract_version}\"\n\n[adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]{capabilities}\n\n\
             [metering]\nsource = \"{metering}\"{config}\n",
            contract_version = ktesio_adapter_api::CONTRACT_VERSION,
            kind = self.kind,
            exec = exec,
            args_toml = args_toml,
            capabilities = capabilities,
            metering = self.metering_source,
            config = config,
        );
        let path = dir.join("adapter.toml");
        std::fs::write(&path, body).expect("write the fixture manifest");
        dir.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// The ONE lag-accumulating subscription drain (both receiver forms)
// ---------------------------------------------------------------------------

/// The `Closed` handling a drain applies — the ONE semantic difference
/// between the two receiver forms' historical behavior (story 7-2):
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClosedPolicy {
    /// A raw `Engine::subscribe()` receiver may outlive its engine (an async
    /// consumer's engine can drop mid-stream): `Closed` ENDS the drain and
    /// the collected tail is returned.
    EndOfStream,
    /// A `Blocking::subscribe()` [`EventSubscription`] keeps its engine's
    /// runtime alive by construction, so `Closed` while a test still drains
    /// is an invariant violation and PANICS (the 7-2 suite's asserted
    /// posture — "the subscription closed while the engine is alive").
    Panic,
}

/// The ONE drain loop. `try_next` is the receiver's `try_recv` under either
/// form; everything else is shared: collect `Ok` events until the tail,
/// ACCUMULATE `Lagged(n)` counts with saturating adds (a drain can pass
/// through more than one lag burst if publishes race the drain, and
/// overwriting would undercount — `Lagged` is not an event), stop on `Empty`
/// per the `closed` policy on `Closed`. Exact, never racy: callers invoke
/// this only AFTER every publishing call has returned (each publish
/// completes under the supervisor lock before its facade call returns), so
/// everything published is already buffered.
pub(crate) fn drain_with(
    mut try_next: impl FnMut() -> Result<EngineEvent, broadcast::error::TryRecvError>,
    closed: ClosedPolicy,
) -> (Vec<EngineEvent>, Option<u64>) {
    let mut events = Vec::new();
    let mut lagged: Option<u64> = None;
    loop {
        match try_next() {
            Ok(event) => events.push(event),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                lagged = Some(lagged.unwrap_or(0).saturating_add(n));
            }
            Err(broadcast::error::TryRecvError::Empty) => return (events, lagged),
            Err(broadcast::error::TryRecvError::Closed) => match closed {
                ClosedPolicy::EndOfStream => return (events, lagged),
                ClosedPolicy::Panic => {
                    panic!("the subscription closed while the engine is alive")
                }
            },
        }
    }
}

/// Drain a RAW `Engine::subscribe()` receiver to its current tail with
/// `try_recv` (the story-7-2 helper for the async subscription surface).
/// Returns the received events plus the TOTAL dropped count if the receiver
/// lagged past the bus capacity. A `Closed` receiver (its engine dropped)
/// simply ends the drain.
pub fn drain_raw_receiver(
    sub: &mut broadcast::Receiver<EngineEvent>,
) -> (Vec<EngineEvent>, Option<u64>) {
    drain_with(|| sub.try_recv(), ClosedPolicy::EndOfStream)
}

/// Drain a `Blocking::subscribe()` [`EventSubscription`] to its current tail.
/// Same Lagged math as [`drain_raw_receiver`] (ONE implementation); the only
/// difference is the `Closed` policy: the subscription holds its engine's
/// runtime, so a closed bus mid-test is an invariant violation and panics.
pub fn drain_subscription(sub: &mut EventSubscription) -> (Vec<EngineEvent>, Option<u64>) {
    drain_with(|| sub.try_recv(), ClosedPolicy::Panic)
}

// ---------------------------------------------------------------------------
// The ONE fake_agent locator (parameterized over the target subdir hop)
// ---------------------------------------------------------------------------

/// Which cargo target subdirectory the running binary resolves the
/// `fake_agent` sibling from: integration-test binaries run from
/// `target/<profile>/deps/`, example binaries from
/// `target/<profile>/examples/`, workspace bins directly from
/// `target/<profile>/` (no marker — passed through unchanged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinDir {
    /// The integration-test `deps/` hop ([`fake_agent_bin`]'s default).
    TestDeps,
    /// The example `examples/` hop (the perf-budgets harness).
    Examples,
}

/// Pure hop resolution, split from the filesystem so it is unit-testable:
/// drop the running binary's file name, then drop the hop's marker directory
/// if present (a binary already at `target/<profile>/` — a workspace bin —
/// passes through). Either way the result is `target/<profile>/`, where the
/// `fake_agent` helper is hardlinked.
fn profile_dir(mut exe: PathBuf, hop: BinDir) -> PathBuf {
    exe.pop(); // drop the binary file name
    let marker = match hop {
        BinDir::TestDeps => "deps",
        BinDir::Examples => "examples",
    };
    if exe.ends_with(marker) {
        exe.pop(); // drop the hop dir → .../target/<profile>/
    }
    exe
}

/// Locate the `fake_agent` test helper binary (story 1.4, AD-3) for a binary
/// running from `hop`, building it on demand when absent. See
/// [`fake_agent_bin`] for the full existence-vs-freshness contract and the
/// CI guard that makes it safe.
pub fn fake_agent_bin_in(hop: BinDir) -> PathBuf {
    let dir = profile_dir(
        std::env::current_exe().expect("locate the running test executable"),
        hop,
    );
    let candidate = dir.join(format!("fake_agent{}", std::env::consts::EXE_SUFFIX));
    if candidate.exists() {
        return candidate;
    }
    // Not built by this harness — build it on demand. EXCLUDED from coverage
    // (the fake_agent bin's own `#[cfg(not(tarpaulin_include))]` precedent):
    // a coverage harness can never honestly execute this arm — the coverage
    // CI job builds the helper explicitly BEFORE the suite, so under
    // tarpaulin the arm is dead by contract, and instrumenting it would only
    // tax the gate with unexecutable lines.
    #[cfg(not(tarpaulin_include))]
    {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut build = std::process::Command::new(&cargo);
        // A shimmed PATH (RUSTC_WRAPPER) must not break the build.
        build
            .args(["build", "-p", "ktesio-conformance", "--bin", "fake_agent"])
            .env_remove("RUSTC_WRAPPER");
        // Match the PROFILE this binary runs in: a release example must not
        // fall back to a debug helper it would never find next to itself
        // (the perf-budgets precedent).
        if dir.ends_with("release") {
            build.arg("--release");
        }
        // Pin the target dir to THIS binary's target root (derived from its
        // own path, which already respects CARGO_TARGET_DIR), so the helper
        // lands exactly where the candidate lives regardless of the invoking
        // shell's cwd.
        if let Some(target_root) = dir.parent() {
            build.arg("--target-dir").arg(target_root);
        }
        let status = build.status();
        if !matches!(status, Ok(s) if s.success() && candidate.exists()) {
            panic!(
                "fake_agent binary not found at {} and an on-demand build did not produce it \
                 (build status: {status:?}). Build `ktesio-conformance` first.",
                candidate.display()
            );
        }
    }
    candidate
}

/// Locate the `fake_agent` test helper binary (story 1.4, AD-3) from a TEST
/// binary ([`BinDir::TestDeps`] — see [`fake_agent_bin_in`] for the
/// parameterized form, and `BinDir::Examples` for the perf-budgets
/// example's hop).
///
/// The engine's start/stop integration tests point a manifest adapter's
/// `[lifecycle.start]` `exec` at this binary so the supervisor spawns a REAL
/// process. `CARGO_BIN_EXE_fake_agent` is only set for THIS crate's own targets,
/// so a cross-crate test resolves the path from the running test executable's
/// location instead: `fake_agent` sits next to the test-deps directory, in the
/// same `debug`/`release` profile dir.
///
/// If the binary is not present (e.g. under `cargo tarpaulin`, which builds test
/// targets but not sibling `[[bin]]` targets), it is BUILT on demand via
/// `cargo build -p ktesio-conformance --bin fake_agent` so the process-spawning
/// tests run under every harness. Panics with a clear message only if the build
/// itself fails.
///
/// # EXISTENCE IS NOT FRESHNESS — the caller's job, and CI's
///
/// This function returns the candidate the moment the file EXISTS. It does not
/// check whether that file was built from the current source, and deliberately
/// so: the on-demand build below is a LAST RESORT, not a routine path. Under
/// `cargo nextest` every test runs in its own process, so a check that decided
/// "stale, rebuild" would fire in many processes at once and serialise them all
/// on cargo's build-directory lock — an observed, reproducible flake (it is why
/// the CI `test` job builds the helper explicitly instead of letting the tests
/// race here).
///
/// The consequence is a trap worth naming, because it has bitten this repo
/// TWICE: a `target/` directory restored from an actions/cache whose key is
/// derived from `Cargo.lock` can hand back a `fake_agent` built before a flag
/// was added. `parse()` in the helper ignores unknown args (`_ => {}`), so a
/// stale binary does not fail — it silently does LESS, and the tests waiting on
/// the output that flag was supposed to produce burn their deadlines and report
/// a timing-shaped failure that has nothing to do with timing.
///
/// The guard therefore lives in `.github/workflows/ci.yml`, in EVERY job that
/// spawns agents: the `test` and `coverage` jobs (`rm -f target/debug/fake_agent
/// target/debug/fake_agent.exe` followed by an explicit `cargo build -p
/// ktesio-conformance --bin fake_agent` before the suite) and the
/// `perf-budgets` job (the same rm + rebuild pair against `target/release/`,
/// where the harness resolves the helper). `scripts/test_automation.py` asserts
/// all three still carry it. Any NEW job that spawns agents must carry it too.
///
/// Kept a plain runtime path computation — no OS-conditional compilation (the
/// executable suffix comes from [`std::env::consts::EXE_SUFFIX`], a runtime
/// constant, so the OS-cfg gate stays green).
pub fn fake_agent_bin() -> PathBuf {
    fake_agent_bin_in(BinDir::TestDeps)
}

/// The manifest wire key for the host OS (the `[capabilities.*]` entries are
/// keyed `linux`/`macos`/`windows` — the engine's `OsId` wire form). Runtime
/// data (matches the engine's `OsId::current()` mapping), not conditional
/// compilation; the `_` arm keeps the same historical `other` fallback kt's
/// local helper carried.
pub fn current_os_key() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Tests (new code ships with tests — and the coverage gate consumes them)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Read the fixture a preset wrote.
    fn toml_of(fixture: ManifestFixture, dir: &Path) -> String {
        let written = fixture.write(dir);
        assert_eq!(written, dir.to_path_buf());
        std::fs::read_to_string(dir.join("adapter.toml")).expect("manifest exists")
    }

    /// A valid `EngineEvent` for the drain tests: the exact breach-record
    /// shape the engine appends to `breaches.log` (the same record
    /// `uj3::read_breach_events` parses), deserialized through the AD-14
    /// wrapper's `kind`-tagged wire form.
    fn probe_event() -> EngineEvent {
        serde_json::from_str(
            r#"{"kind":"budget_breach","schema_version":1,"instance":"probe",
                "run_id":"run-1","scope":"cumulative","dimension":"tokens",
                "limit":90,"observed":90,"action":"pause",
                "metering_source":"self-reported","at":"2026-09-07T00:00:00Z"}"#,
        )
        .expect("the probe event parses")
    }

    #[test]
    fn profile_dir_resolves_both_hops_and_passes_markerless_dirs_through() {
        // A test binary: deps/ popped → the profile dir.
        assert_eq!(
            profile_dir(
                PathBuf::from("/t/debug/deps/agent_cli-abc"),
                BinDir::TestDeps
            ),
            PathBuf::from("/t/debug")
        );
        // An example: examples/ popped → the profile dir.
        assert_eq!(
            profile_dir(
                PathBuf::from("/t/release/examples/perf-budgets"),
                BinDir::Examples
            ),
            PathBuf::from("/t/release")
        );
        // A workspace bin (no marker): passed through — hermes_shim's shape.
        assert_eq!(
            profile_dir(PathBuf::from("/t/debug/hermes_shim"), BinDir::TestDeps),
            PathBuf::from("/t/debug")
        );
        // The hop marker is hop-specific: asking for the examples hop from a
        // deps layout leaves the deps dir (the caller asked for the wrong
        // hop — the marker never matches, exactly like the per-binary code
        // this consolidates).
        assert_eq!(
            profile_dir(PathBuf::from("/t/debug/deps/some_test"), BinDir::Examples),
            PathBuf::from("/t/debug/deps")
        );
    }

    #[test]
    fn frozen_constants_agree_between_the_builder_and_uj3() {
        // The builder's shared defaults and uj3's pinned copies must never
        // drift: both name contract v1, and the uj3_flow preset's metering
        // source is uj3::METERING_SOURCE (not the builder default), which
        // must equal the builder default's wire string — two homes, one
        // pinned equality each way.
        assert_eq!(
            ktesio_adapter_api::CONTRACT_VERSION,
            crate::uj3::CONTRACT_VERSION
        );
        assert_eq!(
            MeteringSource::SelfReported.as_str(),
            crate::uj3::METERING_SOURCE,
            "the builder-default metering source and uj3::METERING_SOURCE are the same wire \
             string (the uj3_flow preset takes the constant explicitly)"
        );
    }

    #[test]
    fn current_os_key_matches_the_engine_wire_keys() {
        assert_eq!(current_os_key(), std::env::consts::OS);
        assert!(matches!(
            current_os_key(),
            "linux" | "macos" | "windows" | "other"
        ));
    }

    #[test]
    fn the_uj3_flow_preset_carries_the_pinned_shape() {
        let dir = TempDir::new().unwrap();
        let text = toml_of(ManifestFixture::uj3_flow(), dir.path());
        assert!(text.contains("contract_version = \"1.0.0\""));
        assert!(text.contains(&format!("kind = \"{}\"", crate::uj3::MANIFEST_KIND)));
        assert!(text.contains("source = \"self-reported\""));
        assert!(text.contains(&format!("--emit-usage\", \"{}\"", crate::uj3::EMIT_EVENTS)));
        assert!(text.contains("--linger-ms\", \"600000\""));
        assert!(text.contains(&format!("env = \"{}\"", crate::uj3::MODEL_ENV_VAR)));
        // Pause + interaction guaranteed on all three modeled OSes: the
        // default pause Breach Action lands the committed `paused` state
        // deterministically everywhere.
        for os in ["linux", "macos", "windows"] {
            assert!(
                text.matches(&format!("{os} = \"guaranteed\"")).count() >= 2,
                "{os} declares both capabilities guaranteed"
            );
        }
    }

    #[test]
    fn the_shape_presets_carry_their_pinned_toml() {
        // One table for the five non-flow presets: what each MUST name on the
        // wire and what it must NOT (the section omissions are load-bearing —
        // a capabilities block changes the per-OS projection the engine reads).
        let crash_state;
        let cases: Vec<(&str, ManifestFixture, Vec<String>, Vec<&str>)> = vec![
            (
                "lingering (both caps, no config)",
                ManifestFixture::lingering("sub-slow"),
                vec![
                    "kind = \"sub-slow\"".into(),
                    "args = [\"--linger-ms\", \"600000\"]".into(),
                ],
                vec!["[config."],
            ),
            (
                "fake_agent (interaction only)",
                ManifestFixture::fake_agent("k", &["--emit-usage", "3"]),
                vec![
                    "[capabilities.interaction]".into(),
                    "args = [\"--emit-usage\", \"3\"]".into(),
                ],
                vec!["[capabilities.pause]", "[config."],
            ),
            (
                "crash_once (readiness-safe delay + state path)",
                {
                    crash_state = std::env::temp_dir().join("ktesio-ts-crash-count");
                    ManifestFixture::crash_once("sub-crashy", &crash_state)
                },
                vec![
                    "args = [\"--crash-after-ms\", \"1500\", \"--crash-times\", \"1\", \
                     \"--crash-state\","
                        .into(),
                    // The crash-state FILENAME only, never the quoted path:
                    // the args are TOML Debug-quoted, so a Windows path's
                    // backslashes are escaped (`C:\\…`) and a full-path
                    // comparison against the raw lossy string would hold on
                    // Unix and fail on Windows. The filename carries no
                    // separators, so it appears verbatim on every OS.
                    crash_state
                        .file_name()
                        .expect("the crash state has a file name")
                        .to_string_lossy()
                        .into_owned(),
                ],
                vec!["[capabilities.pause]"],
            ),
            (
                "replay_batch (re-emits sequence 0)",
                ManifestFixture::replay_batch("sub-replay", 3),
                vec!["args = [\"--emit-usage\", \"3\", \"--replay-usage\", \"--linger-ms\", \"600000\"]".into()],
                vec![],
            ),
            (
                "heartbeat (perf idler: both caps + config.model)",
                ManifestFixture::heartbeat("perfbudgets", 1000, 60_000),
                vec![
                    "args = [\"--heartbeat-ms\", \"1000\", \"--linger-ms\", \"60000\"]".into(),
                    "[config.model]\nenv = \"MODEL\"".into(),
                ],
                vec![],
            ),
        ];
        for (what, fixture, expects, forbids) in cases {
            let dir = TempDir::new().unwrap();
            let text = toml_of(fixture, dir.path());
            for expect in &expects {
                assert!(text.contains(expect.as_str()), "{what}: missing {expect:?}");
            }
            for forbid in &forbids {
                assert!(!text.contains(forbid), "{what}: unexpected {forbid:?}");
            }
            if what.contains("both caps") || what.contains("both caps + config") {
                for os in ["linux", "macos", "windows"] {
                    assert_eq!(
                        text.matches(&format!("{os} = \"guaranteed\"")).count(),
                        2,
                        "{what}: {os} pause + interaction guaranteed"
                    );
                }
            }
        }
    }

    #[test]
    fn single_os_capabilities_merge_into_one_block_per_capability() {
        let dir = TempDir::new().unwrap();
        let fixture = ManifestFixture::new("k", &["--linger-ms", "1"])
            .capability_on_os("interaction", "linux", "guaranteed")
            .capability_on_os("interaction", "windows", "best-effort")
            .capability_on_os("pause", "macos", "guaranteed");
        let text = toml_of(fixture, dir.path());
        // One merged interaction block with both entries, in order…
        let interaction = text
            .split("[capabilities.interaction]\n")
            .nth(1)
            .expect("interaction block")
            .split('\n')
            .take(2)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            interaction,
            "linux = \"guaranteed\"\nwindows = \"best-effort\""
        );
        // …and a separate pause block.
        assert!(text.contains("[capabilities.pause]\nmacos = \"guaranteed\""));
    }

    #[test]
    fn the_kt_current_os_level_shapes_chain_from_the_general_builder() {
        let dir = TempDir::new().unwrap();
        // kt's with_pause shape: CURRENT-OS pause level + interaction ×3.
        let pause_shape = ManifestFixture::new("fake", &["--linger-ms", "600000"])
            .capability_on_os("pause", current_os_key(), "best-effort")
            .guaranteed_on_all_oses("interaction");
        let text = toml_of(pause_shape, dir.path());
        assert!(text.contains(&format!(
            "[capabilities.pause]\n{} = \"best-effort\"",
            current_os_key()
        )));
        for os in ["linux", "macos", "windows"] {
            assert!(
                text.matches(&format!("{os} = \"guaranteed\"")).count() >= 1,
                "{os} interaction guaranteed"
            );
        }
        // kt's with_interaction shape: CURRENT-OS interaction level ONLY.
        let interaction_shape = ManifestFixture::new("fake", &["--linger-ms", "600000"])
            .capability_on_os("interaction", current_os_key(), "guaranteed");
        let text = toml_of(interaction_shape, dir.path());
        assert!(text.contains(&format!(
            "[capabilities.interaction]\n{} = \"guaranteed\"",
            current_os_key()
        )));
        assert!(!text.contains("[capabilities.pause]"));
    }

    #[test]
    fn exec_metering_and_config_overrides_land_in_the_toml() {
        let dir = TempDir::new().unwrap();
        let text = toml_of(
            ManifestFixture::new("k", &["--linger-ms", "1"])
                .exec("/opt/demo-agent")
                .metering("engine-observed")
                .config_env("model", "MODEL"),
            dir.path(),
        );
        assert!(text.contains("exec = \"/opt/demo-agent\""));
        assert!(text.contains("source = \"engine-observed\""));
        assert!(text.contains("[config.model]\nenv = \"MODEL\""));
    }

    #[test]
    fn drain_accumulates_multiple_lag_bursts_and_stops_at_empty() {
        // The ONE Lagged-math implementation, driven as DATA: two bursts in
        // one drain must ACCUMULATE (saturating adds), and overwriting either
        // burst would undercount the dropped prefix.
        let mut steps: Vec<Result<EngineEvent, broadcast::error::TryRecvError>> = vec![
            Err(broadcast::error::TryRecvError::Lagged(2)),
            Err(broadcast::error::TryRecvError::Lagged(3)),
            Err(broadcast::error::TryRecvError::Empty),
        ];
        let (events, lagged) = drain_with(|| steps.remove(0), ClosedPolicy::EndOfStream);
        assert!(events.is_empty(), "no events before the bursts");
        assert_eq!(lagged, Some(5), "bursts accumulate: 2 + 3");

        let mut steps: Vec<Result<EngineEvent, broadcast::error::TryRecvError>> = vec![
            Ok(probe_event()),
            Err(broadcast::error::TryRecvError::Lagged(1)),
            Err(broadcast::error::TryRecvError::Empty),
        ];
        let (events, lagged) = drain_with(|| steps.remove(0), ClosedPolicy::EndOfStream);
        assert_eq!(events.len(), 1);
        assert_eq!(lagged, Some(1));
    }

    #[test]
    fn drain_raw_receiver_ends_on_closed_and_reports_lag_on_a_real_channel() {
        // A REAL broadcast channel: capacity 2, 5 sends → the drain reports
        // Lagged(3) and retains exactly the 2-event window; after the sender
        // drops, the raw receiver's Closed ENDS the drain (EndOfStream).
        let (tx, mut rx) = broadcast::channel::<EngineEvent>(2);
        for _ in 0..5 {
            tx.send(probe_event()).unwrap();
        }
        let (events, lagged) = drain_raw_receiver(&mut rx);
        assert_eq!(events.len(), 2, "the retained window is the capacity");
        assert_eq!(
            lagged,
            Some(3),
            "exactly the events past the window dropped"
        );
        drop(tx);
        let (events, lagged) = drain_raw_receiver(&mut rx);
        assert!(events.is_empty());
        assert_eq!(lagged, None, "a second drain starts from a clean lag book");
    }

    #[test]
    fn drain_subscription_drains_while_alive_and_panics_on_closed() {
        // The EventSubscription form drains the same tail while the engine is
        // alive (here: an empty bus, no lag).
        let dir = TempDir::new().unwrap();
        let engine =
            ktesio_engine::Engine::open(Some(dir.path().to_path_buf())).expect("open engine");
        let mut sub = engine.blocking().subscribe();
        let (events, lagged) = drain_subscription(&mut sub);
        assert!(events.is_empty());
        assert_eq!(lagged, None);
        drop(engine);
        // A live subscription holds its engine's RUNTIME (Arc), so the bus
        // cannot actually close out from under it — which is exactly why the
        // Closed arm is an invariant PANIC, not a result. The panic policy is
        // pinned at the ONE shared loop, driven as data.
        let mut steps: Vec<Result<EngineEvent, broadcast::error::TryRecvError>> =
            vec![Err(broadcast::error::TryRecvError::Closed)];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drain_with(|| steps.remove(0), ClosedPolicy::Panic)
        }));
        assert!(result.is_err(), "Closed on the Panic policy must panic");
    }

    #[test]
    fn fake_agent_bin_resolves_the_test_deps_sibling_name() {
        // The fast path (or the on-demand build, on a cold target): either
        // way the resolved name is the suffixed helper next to the profile
        // dir. Existence is NOT asserted here — a coverage/CI host may not
        // have built it before this unit test runs (the function's own
        // on-demand arm covers that case).
        let bin = fake_agent_bin();
        assert_eq!(
            bin.file_name(),
            Some(std::ffi::OsStr::new(
                format!("fake_agent{}", std::env::consts::EXE_SUFFIX).as_str()
            )),
            "the resolved sibling is the suffixed fake_agent helper"
        );
        assert!(
            !bin.to_string_lossy().ends_with("deps"),
            "the deps hop must pop to the profile dir: {}",
            bin.display()
        );
    }
}
