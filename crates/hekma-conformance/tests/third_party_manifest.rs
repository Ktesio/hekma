//! The THIRD-PARTY adapter simulation (story 6-4, AC line 4): this file is what
//! an external adapter crate's dev-test looks like. It compiles against
//! `hekma-conformance` ALONE (this crate's own manifest adapter is the
//! third-party shape — a directory holding an `adapter.toml` that execs the
//! conformance `fake_agent`), imports ONLY the public TCK surface, and asserts
//! on the returned [`ConformanceReport`] — the "cargo test harness any
//! third-party adapter crate can invoke" contract, with no private helper, no
//! test-runner framework, and no macros.
//!
//! The manifest below is authored from scratch (the way a real adapter author
//! would write theirs): a launchable `[lifecycle.start]` carrying the harness's
//! `--dump <path>` delivery-evidence pair, a per-OS Capability Declaration, a
//! self-reported Metering Source, and the declared `[config]` rules (the
//! documented `model` key plus the reserved `memory.dir` key) whose delivery
//! the report proves.

use std::path::Path;

use hekma_conformance::{run_mock_conformance, section_ids, SectionResult};

/// Write one third-party adapter: `<dir>/adapter.toml` pointing the launch at
/// the conformance `fake_agent` (resolved through the crate's public binary
/// seam — the same one the engine's integration tests use).
fn write_adapter(dir: &Path) {
    let exec = hekma_conformance::fake_agent_bin();
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "third-party-adapter"

[lifecycle.start]
exec = {exec:?}
args = ["--dump", "config-dump.txt", "--linger-ms", "600000"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[config.model]
env = "MODEL"

[config."memory.dir"]
env = "THIRD_PARTY_MEMORY"
"#
    );
    std::fs::write(dir.join("adapter.toml"), body).expect("write adapter.toml");
}

/// A third-party adapter crate's conformance `#[test]`: run every section
/// against ITS adapter and assert the report.
#[test]
fn third_party_manifest_adapter_is_proven_conformant_by_the_public_harness() {
    let dir = tempfile::Builder::new()
        .prefix("ktesio-third-party-")
        .tempdir()
        .expect("tempdir");
    write_adapter(dir.path());

    // The whole integration is ONE call: register with a fresh engine, drive
    // every section, get the machine-readable report back.
    let report = run_mock_conformance(dir.path());

    // The report names the adapter under test and carries the FULL fixed
    // section set (the machine-readable contract is complete — a failed
    // section never truncates the suite).
    assert_eq!(report.adapter_kind, "third-party-adapter");
    let expected_order = [
        section_ids::CAPABILITY_EDGES,
        section_ids::LIFECYCLE,
        section_ids::PAUSE,
        section_ids::CONFIG_MAPPING,
        section_ids::METERING_SELF_REPORTED,
        section_ids::METERING_ENGINE_OBSERVED,
        section_ids::MEMORY,
        section_ids::INTERACTION,
    ];
    let got: Vec<&str> = report
        .sections
        .iter()
        .map(|s| s.section.as_str())
        .collect::<Vec<_>>();
    assert_eq!(got, expected_order, "fixed section order");

    // Conformance verdict: everything applicable passed.
    assert!(report.is_conformant(), "failures = {:?}", report.failures());

    // Per-section semantics, per the declaration. PAUSE is special: on an
    // unmodeled host (OsId::Other) the projection reads unsupported — an
    // honest not_applicable naming the declaration — while every modeled OS
    // must demonstrate the pause path.
    for id in [
        section_ids::CAPABILITY_EDGES,
        section_ids::LIFECYCLE,
        section_ids::CONFIG_MAPPING,
        section_ids::METERING_SELF_REPORTED,
        section_ids::MEMORY,
        section_ids::INTERACTION,
    ] {
        assert_eq!(
            report.section(id),
            Some(&SectionResult::Pass),
            "{id} must pass for a conforming declaration"
        );
    }
    if hekma_adapter_api::OsId::current() == hekma_adapter_api::OsId::Other {
        match report.section(section_ids::PAUSE) {
            Some(SectionResult::NotApplicable { reason }) => {
                assert!(reason.contains("unsupported"), "{reason}");
            }
            other => panic!("on Other, pause must read not_applicable, got {other:?}"),
        }
    } else {
        assert_eq!(
            report.section(section_ids::PAUSE),
            Some(&SectionResult::Pass),
            "pause must pass for a conforming declaration on a modeled OS"
        );
    }
    // The one honest skip: a SelfReported declaration never owes
    // EngineObserved proof — and the reason NAMES the declaration.
    match report.section(section_ids::METERING_ENGINE_OBSERVED) {
        Some(SectionResult::NotApplicable { reason }) => {
            assert!(
                reason.contains("self-reported"),
                "the skip reason must name the declaration: {reason}"
            );
        }
        other => panic!("expected NotApplicable for engine-observed, got {other:?}"),
    }

    // The report is machine-readable per capability: it serializes to JSON
    // whose entries carry a `status` tag (the shape a CI gate consumes).
    let json = serde_json::to_string(&report).expect("report serializes");
    assert!(json.contains("\"schema_version\":1"), "{json}");
    // The negotiated contract version rides on every report (retro #163, B8).
    assert!(json.contains("\"contract_version\":\"1.0.0\""), "{json}");
    assert!(json.contains("\"adapter_kind\":\"third-party-adapter\""));
    assert!(json.contains("\"status\":\"pass\""));
    assert!(json.contains("\"status\":\"not_applicable\""));
    assert!(!json.contains("\"status\":\"fail\""));
}
