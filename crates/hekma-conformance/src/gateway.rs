//! The REFERENCE gateway manifest (story 18-5) — the one `adapter.toml` that
//! declares everything epic 18 shipped, as a real service-shaped agent would.
//!
//! A gateway (an API server behind a supervisor) is the canonical service-mode
//! agent: it opens a loopback HTTP port and only then is ready (`[readiness]`
//! on its models endpoint), it proves it is alive by rewriting a heartbeat
//! file (`[liveness.heartbeat_file]`), it uses the conventional exit-75
//! "please relaunch me" contract (`[exit_codes]`), and it drains cooperatively
//! — a pause/resume command pair plus an idle marker the drain verb waits on
//! (`[lifecycle.pause]`/`[lifecycle.resume]` + `[drain]`).
//!
//! This module is the EXECUTABLE single source of truth for that manifest:
//! [`gateway_manifest_text`] composes it (the start command is ONE
//! `fake_agent` invocation whose existing fixture flags already behave like
//! every part of a gateway — composition over construction), and
//! [`write_gateway_manifest`] materializes it as a TCK subject. The doc
//! example in `docs/manifest.md` shows the SAME shape (the docs pin below
//! parses that example with the real manifest parser and pins its section
//! set against the kit's; the end-to-end run lives in the crate's
//! `tests/gateway_e2e.rs` — the 18-5 review's C7 split: the src pins are
//! the cheap static truth, the integration proof runs as a test target).
//!
//! The five declarations, each the 18-x story it closes:
//!
//! * `[readiness]` `http-get` on the models endpoint — story 18-1.
//! * `[liveness.heartbeat_file]` — story 18-2.
//! * `[exit_codes]` `restart_requested = [75]` — story 18-3.
//! * `[lifecycle.pause]`/`[lifecycle.resume]` (the sentinel-touch pair) —
//!   story 18-4's cooperative half.
//! * `[drain]` (`idle_file` + `timeout_ms`) — story 18-4's bounded-wait half.
//!
//! Beyond the five, the manifest carries the two DELIVERY pairs the TCK's
//! remaining sections prove on the subject itself — `[config.model]` (an
//! operator key → env, proven through the subject's `--dump` artifact) and
//! the reserved `[config."memory.dir"]` (Memory Backing delivery) — plus the
//! `--dump` seam those proofs read. Without them those sections would read
//! not_applicable (honest, but a weaker reference); WITH them the reference
//! is a FULLY conforming subject: the epic's integration proof runs every
//! section that can apply.
//!
//! Test infrastructure — never a driving surface (the `test_support` module
//! boundary applies verbatim).

use std::net::TcpListener;
use std::path::{Path, PathBuf};

/// The gateway subject's `[adapter].kind` — the name the TCK's end-to-end run
/// labels the report with.
pub const GATEWAY_KIND: &str = "tck-gateway";

/// The Adapter Contract version the reference manifest targets (18-5 review
/// round 1, C1) — the ONE constant the composed text's `contract_version`
/// line names. A contract bump that forgets the reference breaks the pins
/// that read the version back through the parsed manifest.
pub const GATEWAY_CONTRACT_VERSION: &str = "1.1.0";

/// The gateway fixture's late-binding readiness delay (milliseconds — the
/// fake agent's `--http-ready-after`, 18-5 review round 1, A2): the models
/// endpoint binds only after this delay, so the declared hold is REAL — the
/// start sits in `starting` until the endpoint answers — and the readiness
/// section's healthy leg can OBSERVE the hold instead of asserting only the
/// final adapter-ready edge. `docs/manifest.md`'s hand-written example does
/// NOT carry the flag: it is fixture-convention, not contract.
pub const GATEWAY_READY_AFTER_MS: u64 = 900;

/// The gateway fixture's HTTP status on its models endpoint (the ONE status
/// the declared readiness probe counts as ready).
pub const GATEWAY_READY_STATUS: u16 = 200;

/// The heartbeat rewrite interval the gateway fixture beats at (milliseconds).
/// Comfortably below the declared liveness window: ten missed beats would
/// still not fail the agent.
pub const GATEWAY_HEARTBEAT_INTERVAL_MS: u64 = 100;

/// The gateway's declared liveness window (`[liveness.heartbeat_file]`
/// `max_age_ms`). Well above the beat interval and above the engine's reaper
/// tick, so a healthy run never fails and a declared window is observable.
pub const GATEWAY_LIVENESS_WINDOW_MS: u64 = 1500;

/// The gateway's declared readiness budget (`[readiness].timeout_ms`).
/// Generous through a loaded CI runner; the endpoint binds within the first
/// moments of the process, so the gate normally opens in one or two polls.
pub const GATEWAY_READINESS_TIMEOUT_MS: u64 = 8000;

/// The gateway's declared drain bound (`[drain].timeout_ms`). The idle signal
/// lands [`GATEWAY_IDLE_DELAY_MS`] after the pause sentinel, so a healthy
/// drain completes a comfortable order of magnitude inside the bound.
pub const GATEWAY_DRAIN_BOUND_MS: u64 = 10000;

/// How long after the PAUSE sentinel appears the gateway fixture touches its
/// idle marker (the `--idle-on-pause` delay) — the agent's own "my work
/// stopped" latency, anchored to the pause the DRAIN's command delivers so
/// the signal cannot predate the drain.
pub const GATEWAY_IDLE_DELAY_MS: u64 = 800;

/// How long after the PAUSE sentinel appears the gateway fixture ends itself
/// (the `--exit-on-pause` delay, CANCELLED by the resume sentinel) — the
/// Windows graceful-stop fixture applied to the reference subject itself.
/// The engine's Windows stop ladder has no deliverable graceful signal: its
/// graceful step is a pure exit-watch, so a never-ending subject escalates to
/// `stop-forced` after the whole default window — a terminal cause the drain
/// section's contract rightly rejects (it must record `drain-stopped`). The
/// anchored self-exit ends the subject on its own inside the graceful window
/// on every OS, without touching the wait's truth: the idle signal still
/// lands at anchor+800ms, the drain's bounded wait completes exactly as
/// before, and only the stop's exit-watch concludes early. The delay sits
/// well past the idle signal (the wait's proof is untouched) and well inside
/// the stop ladder's default graceful window; the CANCEL anchor (the resume
/// sentinel) stands the self-exit down in every section that pauses and
/// RESUMES the subject (the pause section), and a stale anchor at a later
/// section's process start disarms immediately against the stale resume
/// sentinel — only the drain's never-resumed pause ends the process. On
/// Unix the drain's stop is a delivered signal long before this delay, so
/// nothing changes there.
pub const GATEWAY_EXIT_ON_PAUSE_MS: u64 = 6000;

/// The Agent-Home-relative files the gateway fixture's signals live at (the
/// manifest declares the same names the start command's flags write).
pub const GATEWAY_HEARTBEAT_FILE: &str = "gateway.heartbeat";
pub const GATEWAY_IDLE_FILE: &str = "gateway.idle";
pub const GATEWAY_PAUSE_SENTINEL: &str = "pause.sentinel";
pub const GATEWAY_RESUME_SENTINEL: &str = "resume.sentinel";
/// The work log the gateway fixture's `--watch-sentinel` thread appends to.
pub const GATEWAY_WORK_LOG: &str = "gateway.log";
/// The dump artifact the start command's `--dump` seam writes (the config and
/// memory sections' delivery proof reads the subject's OWN dump).
pub const GATEWAY_DUMP_FILE: &str = "config-dump.txt";
/// The env var the gateway's `model` key maps to (the config section sets and
/// proves it through the dump).
pub const GATEWAY_MODEL_ENV: &str = "GATEWAY_MODEL";
/// The env var the reserved `memory.dir` key maps to (the engine injects the
/// managed dir; the memory section proves it through the dump).
pub const GATEWAY_MEMORY_DIR_ENV: &str = "GATEWAY_MEMORY_DIR";

/// The canonical gateway manifest TEXT for a loopback `port` — the kit's
/// single source of truth (the docs example shows the same five
/// declarations). `port` is the only per-run input: the readiness probe and
/// the start command's `--http-ready` must name the SAME listener, and a test
/// materializing the subject binds an ephemeral port first (the standard
/// test-practice TOCTOU window — see [`free_loopback_port`]).
///
/// The composed start command is ONE `fake_agent` process carrying every
/// gateway behavior at once: the late-binding readiness delay
/// (`--http-ready-after` — the hold the readiness leg observes), the
/// models-endpoint server (`--http-ready`), the heartbeat
/// (`--heartbeat-file`), the cooperative work loop (`--watch-sentinel`), the
/// pause-anchored idle signal (`--idle-on-pause`), the pause-anchored
/// self-exit with its resume-cancel (`--exit-on-pause` — the Windows
/// graceful-stop fixture, see [`GATEWAY_EXIT_ON_PAUSE_MS`]), the
/// config/memory delivery-evidence seam (`--dump`), and a linger longer than
/// anything the TCK drives. No fixture flag conflicts
/// with another: each arms its own thread at startup (verified — story 18-5's
/// composition check), so no unifying flag was needed.
pub fn gateway_manifest_text(port: u16) -> String {
    let bin = crate::fake_agent_bin();
    // A non-UTF-8 exec path cannot ride the TOML wire — fail loudly (the
    // `ManifestFixture` write path's precedent).
    let exec = bin.to_str().unwrap_or_else(|| {
        panic!(
            "the fake_agent path {} is not valid UTF-8; the TOML manifest requires a UTF-8 path",
            bin.display()
        )
    });
    let exec = exec.replace('\\', "/");
    format!(
        r#"contract_version = "{GATEWAY_CONTRACT_VERSION}"

[adapter]
kind = "{GATEWAY_KIND}"

[lifecycle.start]
exec = "{exec}"
args = [
  "--http-ready-after", "{GATEWAY_READY_AFTER_MS}",
  "--http-ready", "{port}:{GATEWAY_READY_STATUS}",
  "--heartbeat-file", "{GATEWAY_HEARTBEAT_FILE}:{GATEWAY_HEARTBEAT_INTERVAL_MS}",
  "--watch-sentinel", "{GATEWAY_PAUSE_SENTINEL}:{GATEWAY_RESUME_SENTINEL}:{GATEWAY_WORK_LOG}",
  "--idle-on-pause", "{GATEWAY_IDLE_DELAY_MS}:{GATEWAY_PAUSE_SENTINEL}:{GATEWAY_IDLE_FILE}",
  "--exit-on-pause", "{GATEWAY_EXIT_ON_PAUSE_MS}:{GATEWAY_PAUSE_SENTINEL}:{GATEWAY_RESUME_SENTINEL}",
  "--dump", "{GATEWAY_DUMP_FILE}",
  "--linger-ms", "600000",
]

[lifecycle.pause]
exec = "{exec}"
args = ["--touch", "{GATEWAY_PAUSE_SENTINEL}"]

[lifecycle.resume]
exec = "{exec}"
args = ["--touch", "{GATEWAY_RESUME_SENTINEL}"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[config.model]
env = "{GATEWAY_MODEL_ENV}"

[config."memory.dir"]
env = "{GATEWAY_MEMORY_DIR_ENV}"

[readiness]
kind = "http-get"
timeout_ms = {GATEWAY_READINESS_TIMEOUT_MS}
url = "http://127.0.0.1:{port}/models"
expected_status = {GATEWAY_READY_STATUS}

[liveness.heartbeat_file]
path = "{GATEWAY_HEARTBEAT_FILE}"
max_age_ms = {GATEWAY_LIVENESS_WINDOW_MS}

[exit_codes]
restart_requested = [75]

[drain]
timeout_ms = {GATEWAY_DRAIN_BOUND_MS}
idle_file = "{GATEWAY_IDLE_FILE}"
"#
    )
}

/// Materialize the reference gateway manifest as `<dir>/adapter.toml`
/// (created if missing) and return the directory — the shape both
/// `AdapterRef::Manifest` and `kt agent register --manifest` accept. This is
/// how the TCK's end-to-end run registers the subject.
pub fn write_gateway_manifest(dir: &Path, port: u16) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap_or_else(|e| {
        panic!(
            "could not create the gateway manifest dir {}: {e}",
            dir.display()
        )
    });
    let path = dir.join("adapter.toml");
    std::fs::write(&path, gateway_manifest_text(port))
        .unwrap_or_else(|e| panic!("write the gateway manifest {}: {e}", path.display()));
    dir.to_path_buf()
}

/// Bind an ephemeral loopback port and immediately drop the listener,
/// returning the port — the standard test-practice allocation for a manifest
/// whose probe URL and start command must name the same listener. The usual
/// bind-race window applies (another process could claim the port before the
/// subject spawns); a test runs its subject immediately after allocating, and
/// the declared readiness budget absorbs any rare re-bind by retrying logic
/// at the call site.
pub fn free_loopback_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral loopback port")
        .local_addr()
        .expect("read the bound port")
        .port()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::hekma_adapter_api::Manifest;

    /// THE SECTION-SET PIN: the reference manifest declares EXACTLY the five
    /// epic-18 service declarations — the readiness probe, the liveness
    /// heartbeat, the exit-code classes, the pause/resume command pair, and
    /// the drain — and nothing more. A sixth service declaration or a dropped
    /// one breaks the pin: the kit's reference IS the epic's integration
    /// shape, and the docs example shows the same set. (The `[config]` block
    /// is NOT a service declaration: it is the delivery pair — `model` +
    /// the reserved `memory.dir` — the pre-existing config/memory sections
    /// prove on the subject, and it is pinned to exactly those two keys.)
    #[test]
    fn the_reference_manifest_declares_exactly_the_five_epic_sections() {
        let manifest = gateway_manifest();
        // The five, each present and SHAPED as its story declared it.
        let readiness = manifest
            .readiness_probe()
            .expect("readiness resolves")
            .expect("the reference declares [readiness]");
        assert!(
            matches!(
                readiness.probe,
                ::hekma_adapter_api::ReadinessProbe::HttpGet(_)
            ),
            "the readiness declaration is the models-endpoint http-get"
        );
        let liveness = manifest
            .liveness_check()
            .expect("liveness resolves")
            .expect("the reference declares [liveness]");
        assert!(
            matches!(
                liveness,
                ::hekma_adapter_api::LivenessCheck::HeartbeatFile(_)
            ),
            "the liveness declaration is the heartbeat file"
        );
        let exit_codes = manifest
            .exit_codes()
            .expect("exit codes resolve")
            .expect("the reference declares [exit_codes]");
        assert_eq!(
            exit_codes.restart_requested,
            vec![75],
            "the reference declares the conventional exit-75 relaunch ask"
        );
        assert!(
            exit_codes.clean.is_empty(),
            "the reference declares no clean class (the drain's stop is the engine's)"
        );
        let drain = manifest
            .drain()
            .expect("drain resolves")
            .expect("the reference declares [drain]");
        assert_eq!(
            drain.idle_file.as_deref(),
            Some(GATEWAY_IDLE_FILE),
            "the drain waits on the gateway's idle marker"
        );
        let lifecycle = manifest.lifecycle.as_ref().expect("the [lifecycle] block");
        assert!(
            lifecycle.start.is_some(),
            "the reference is launchable (the composed start command)"
        );
        assert!(
            lifecycle.pause.is_some() && lifecycle.resume.is_some(),
            "the reference declares the cooperative pause/resume PAIR"
        );
        // C3 (18-5 review round 1): the reference declares NO [lifecycle.stop]
        // template — the engine's stop ladder is the contract; a manifest
        // stop command would put the reference's stops outside it.
        assert!(
            lifecycle.stop.is_none(),
            "the reference declares no [lifecycle.stop] override"
        );
        // C3: the capability table declares EXACTLY the two capabilities the
        // TCK's sections demonstrate — pause + interaction — on every modeled
        // OS (no third capability sneaks in, none is dropped).
        for os in ::hekma_adapter_api::OsId::MODELED {
            let mut keys: Vec<::hekma_adapter_api::Capability> = manifest
                .capabilities
                .as_ref()
                .expect("the [capabilities] block")
                .effective(os)
                .entries
                .into_iter()
                .map(|(capability, _)| capability)
                .collect();
            keys.sort();
            keys.dedup();
            assert_eq!(
                keys,
                vec![
                    ::hekma_adapter_api::Capability::Pause,
                    ::hekma_adapter_api::Capability::Interaction,
                ],
                "the capability table on {os:?} is exactly interaction + pause"
            );
        }
        // And NOTHING beyond the five + the delivery pair: no [interaction]
        // channel override; the [config] block carries EXACTLY the two keys
        // the config/memory sections prove (an operator key + the reserved
        // engine-computed one) — no third mapping sneaks in.
        let config = manifest.config.as_ref().expect("the [config] block");
        let keys: Vec<&str> = config.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["memory.dir", "model"],
            "the delivery pair is exactly model + the reserved memory.dir"
        );
        assert!(
            manifest.interaction.is_none(),
            "the reference declares no [interaction] channel override"
        );
    }

    /// THE VALIDATE PIN: the reference is valid BY CONSTRUCTION —
    /// `Manifest::validate` passes on the exact text the kit materializes
    /// (port included). A schema or validation change that would reject the
    /// reference fails here, not in a downstream run.
    #[test]
    fn the_reference_manifest_is_valid_by_construction() {
        let manifest = gateway_manifest();
        manifest
            .validate()
            .expect("the reference gateway manifest must pass Manifest::validate");
        // C1: the composed text's contract line IS the constant (a drift
        // between the two is a silently mis-targeted contract).
        assert_eq!(
            gateway_manifest_text(48081)
                .lines()
                .next()
                .expect("the manifest text is non-empty"),
            format!("contract_version = \"{GATEWAY_CONTRACT_VERSION}\""),
            "the first line names the contract-version constant"
        );
    }

    /// THE DOCS PIN (18-5 review round 1, C2): `docs/manifest.md`'s Complete
    /// Example is the SAME section set the kit's reference declares — parsed
    /// with the REAL manifest parser and pinned against the same set the kit
    /// pin above asserts. The docs example is hand-shaped for readability
    /// (its exec is a bare `my-gateway`, its port a fixed 8080, its args the
    /// generic story flags), so the equality is the SECTION SET, never the
    /// bytes: the docs show the kit's shape, not a parallel dialect.
    #[test]
    fn the_docs_example_declares_the_reference_section_set() {
        let docs_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/manifest.md");
        let docs = std::fs::read_to_string(&docs_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", docs_path.display()));
        // The Complete Example's ```toml fence — the doc's FIRST toml fence.
        let fence = docs
            .split("```toml")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("docs/manifest.md carries a ```toml fence (the Complete Example)");
        let manifest = Manifest::from_toml_str(fence)
            .expect("the docs example parses with the REAL manifest parser");
        manifest
            .validate()
            .expect("the docs example validates against the real schema");

        // The same section set the kit pin pins (each assert mirrors its
        // kit-pin counterpart):
        assert!(
            manifest
                .readiness_probe()
                .expect("readiness resolves")
                .is_some(),
            "the docs example declares [readiness]"
        );
        assert!(
            matches!(
                manifest.liveness_check().expect("liveness resolves"),
                Some(::hekma_adapter_api::LivenessCheck::HeartbeatFile(_))
            ),
            "the docs example declares the heartbeat-file liveness"
        );
        let exit_codes = manifest
            .exit_codes()
            .expect("exit codes resolve")
            .expect("the docs example declares [exit_codes]");
        assert_eq!(exit_codes.restart_requested, vec![75]);
        assert!(exit_codes.clean.is_empty());
        assert!(manifest.drain().expect("drain resolves").is_some());
        let lifecycle = manifest.lifecycle.as_ref().expect("the [lifecycle] block");
        assert!(lifecycle.start.is_some());
        assert!(lifecycle.pause.is_some() && lifecycle.resume.is_some());
        assert!(
            lifecycle.stop.is_none(),
            "the docs example, like the reference, overrides no stop template"
        );
        assert!(
            manifest.interaction.is_none(),
            "the docs example declares no [interaction] channel override"
        );
        let config = manifest.config.as_ref().expect("the [config] block");
        let keys: Vec<&str> = config.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["memory.dir", "model"]);
        for os in ::hekma_adapter_api::OsId::MODELED {
            let mut capability_keys: Vec<::hekma_adapter_api::Capability> = manifest
                .capabilities
                .as_ref()
                .expect("the [capabilities] block")
                .effective(os)
                .entries
                .into_iter()
                .map(|(capability, _)| capability)
                .collect();
            capability_keys.sort();
            capability_keys.dedup();
            assert_eq!(
                capability_keys,
                vec![
                    ::hekma_adapter_api::Capability::Pause,
                    ::hekma_adapter_api::Capability::Interaction,
                ],
                "the docs example's capability table on {os:?} is exactly interaction + pause"
            );
        }
    }

    /// The write helper materializes EXACTLY the canonical text (the helper
    /// and the text fn cannot drift apart), and the materialized file
    /// re-parses + re-validates from disk — the round trip a TCK run
    /// actually performs.
    #[test]
    fn write_gateway_manifest_materializes_the_canonical_text() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-gateway-pin-")
            .tempdir()
            .expect("tempdir");
        let port = 48081;
        write_gateway_manifest(dir.path(), port);
        let on_disk = std::fs::read_to_string(dir.path().join("adapter.toml"))
            .expect("read the materialized manifest");
        assert_eq!(
            on_disk,
            gateway_manifest_text(port),
            "the materialized file IS the canonical text"
        );
        let manifest = Manifest::from_toml_str(&on_disk).expect("re-parse from disk");
        manifest
            .validate()
            .expect("the materialized manifest validates");
        assert_eq!(
            manifest.adapter.expect("adapter identity").kind,
            GATEWAY_KIND
        );
    }

    /// The composed start command's port and the readiness probe's URL name
    /// the SAME listener for any port — a mismatched pair would prove the
    /// readiness gate against an endpoint the subject never opens (a
    /// manifest that can never pass its own gate).
    #[test]
    fn the_probe_url_and_the_start_command_name_the_same_port() {
        for port in [1, 9000, 48081, 65535] {
            let text = gateway_manifest_text(port);
            assert!(
                text.contains(&format!("--http-ready\", \"{port}:")),
                "the start command binds port {port}"
            );
            assert!(
                text.contains(&format!("url = \"http://127.0.0.1:{port}/models\"")),
                "the readiness probe dials port {port}"
            );
        }
    }

    /// Parse + validate once for the set pins (the resolved sections come
    /// from `validate`'s work).
    fn gateway_manifest() -> Manifest {
        let text = gateway_manifest_text(48081);
        let manifest = Manifest::from_toml_str(&text).expect("the reference parses");
        manifest
            .validate()
            .expect("the reference validates before the set pins read it");
        manifest
    }
}
