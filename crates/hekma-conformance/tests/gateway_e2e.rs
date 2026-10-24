//! THE GATEWAY END-TO-END PROOF (story 18-5; review round 1 C4/C5/C7): the
//! reference gateway subject — [`write_gateway_manifest`]'s canonical
//! manifest — driven by the REAL harness comes out conformant with every
//! applicable section passing, its run-level evidence populated, and its
//! work log proving the agent actually worked.
//!
//! Why an integration test target (C7): the src-side pins are the cheap
//! static truth (the section set, `validate`, the docs shape); this target
//! is the expensive live run — a real engine, a real subprocess, every
//! section — and it stays out of the library's unit-test binary so a
//! `cargo test --lib` stays fast.
//!
//! The C4 retry: the manifest binds an EPHEMERAL port that the subject only
//! claims at spawn (the standard test-practice TOCTOU window), so on a
//! loaded runner another process can claim the port between
//! [`free_loopback_port`] and the spawn — the readiness probe then never
//! passes and the run fails naming the readiness gate. That ONE symptom is
//! retryable: a fresh attempt gets a fresh port and a fresh state root. Any
//! OTHER failure is a real defect and returns as-is (a blanket retry would
//! launder flaky assertions into passes).
//!
//! The C5 work-log proof: the run uses a CALLER-OWNED state root
//! ([`TckConfig::state_root`]), and the subject's own `--watch-sentinel`
//! work log is read back through the engine's public path layout
//! ([`EnginePaths`]) — `work <n>` lines prove the agent did work, not just
//! that its lifecycle states moved.

use hekma_conformance::gateway::{
    free_loopback_port, write_gateway_manifest, GATEWAY_KIND, GATEWAY_WORK_LOG,
};
use hekma_conformance::{
    run_conformance_with_evidence, section_ids, ConformanceReport, SectionResult,
    ServiceRunEvidence, TckAdapter, TckConfig,
};
use hekma_engine::domain::InstanceName;
use hekma_engine::paths::EnginePaths;

/// The bind-race retry budget (C4): three attempts, and only the readiness
/// bind-race symptom may trigger one.
const BIND_RACE_ATTEMPTS: usize = 3;

/// One full harness run over the reference gateway on a caller-owned root.
/// The manifest is materialized in its OWN temp dir: the state root must
/// stay empty (exclusively one run's state), so the manifest file cannot
/// live inside it.
fn run_gateway_once(root: &std::path::Path) -> (ConformanceReport, ServiceRunEvidence) {
    let manifest_dir = tempfile::Builder::new()
        .prefix("ktesio-gateway-e2e-manifest-")
        .tempdir()
        .expect("tempdir");
    let port = free_loopback_port();
    let manifest = write_gateway_manifest(manifest_dir.path(), port);
    let config = TckConfig {
        state_root: Some(root.to_path_buf()),
        ..TckConfig::default()
    };
    run_conformance_with_evidence(&TckAdapter::Manifest(manifest), &config)
}

/// Whether a non-conformant report carries THE retryable symptom (C4): a
/// readiness-section failure naming the probe gate never passing — the
/// bind-race shape (a claimed port means the endpoint never answers, the
/// agent's bind thread honestly gives up, and the probe times out naming
/// itself). Everything else is a real defect.
fn is_readiness_bind_race(report: &ConformanceReport) -> bool {
    match report.section(section_ids::READINESS) {
        Some(SectionResult::Fail { reason }) => {
            reason.contains("readiness probe") || reason.contains("readiness-timeout")
        }
        _ => false,
    }
}

/// THE EPIC'S INTEGRATION PROOF: the reference gateway subject comes out
/// conformant with every applicable section passing — all FOUR new service
/// sections demonstrated end-to-end on the declared subject, the ONE
/// declaration-justified metering skip pinned, the run-level evidence
/// POPULATED next to every service pass (A1: a deleted validator call can
/// no longer pass silently), and the subject's own work log read back
/// through the engine's path layout (C5).
#[test]
fn the_reference_gateway_passes_the_whole_conformance_suite() {
    let mut last = None;
    for attempt in 1..=BIND_RACE_ATTEMPTS {
        let root = tempfile::Builder::new()
            .prefix("ktesio-gateway-e2e-")
            .tempdir()
            .expect("tempdir");
        let (report, evidence) = run_gateway_once(root.path());
        if report.is_conformant() {
            assert_gateway_suite(&report, &evidence, root.path());
            return;
        }
        assert!(
            is_readiness_bind_race(&report),
            "attempt {attempt} failed with a NON-retryable defect (failures = {:?}); \
             only the readiness bind-race symptom may retry",
            report.failures()
        );
        last = Some(report);
    }
    panic!(
        "the gateway e2e failed {BIND_RACE_ATTEMPTS} attempts, each the readiness bind-race \
         symptom (failures = {:?}) — a flaky port allocation, not a one-off",
        last.expect("at least one attempt ran").failures()
    );
}

/// The full-run assertions (factored so the retry loop above stays readable).
fn assert_gateway_suite(
    report: &ConformanceReport,
    evidence: &ServiceRunEvidence,
    root: &std::path::Path,
) {
    assert_eq!(
        report.adapter_kind, GATEWAY_KIND,
        "the report labels the reference subject"
    );
    assert_eq!(
        report.sections.len(),
        12,
        "the complete fixed section set ran (a failure never truncates the suite)"
    );
    assert!(
        report.is_conformant(),
        "the reference gateway must conform: failures = {:?}",
        report.failures()
    );

    // THE four new sections: each PASSES on the declaring subject — the
    // epic's readiness gate, liveness watch, exit classes, and drain
    // composite, proven end-to-end in one run.
    for id in [
        section_ids::DRAIN,
        section_ids::READINESS,
        section_ids::LIVENESS,
        section_ids::EXIT_CLASSES,
    ] {
        assert_eq!(
            report.section(id),
            Some(&SectionResult::Pass),
            "{id} must PASS for the fully-declaring reference subject"
        );
    }

    // The pre-existing sections the delivery pair + capabilities earn:
    // every one applicable to the reference runs and passes too.
    for id in [
        section_ids::CAPABILITY_EDGES,
        section_ids::LIFECYCLE,
        section_ids::PAUSE,
        section_ids::CONFIG_MAPPING,
        section_ids::METERING_SELF_REPORTED,
        section_ids::MEMORY,
        section_ids::INTERACTION,
    ] {
        assert_eq!(
            report.section(id),
            Some(&SectionResult::Pass),
            "{id} must PASS for the reference subject (its declaration earns the demo)"
        );
    }

    // The ONE declaration-justified skip, pinned with its exact reason
    // (the mirror image of the engine-observed subject's pin).
    assert_eq!(
        report.section(section_ids::METERING_ENGINE_OBSERVED),
        Some(&SectionResult::NotApplicable {
            reason: "the declaration declares self-reported metering, so the \
                     engine-observed section does not apply"
                .to_string()
        })
    );

    // THE EVIDENCE RECEIPT (A1): every service section that just passed
    // recorded the observables its validators verified. A validator call
    // deleted anywhere in the pipeline leaves its receipt at `None` and
    // dies HERE, next to the report it would otherwise hollow out.
    let drain = &evidence.drain;
    assert_eq!(
        drain.subject_completed,
        Some(true),
        "the drain subject leg recorded its outcome"
    );
    assert!(
        drain.subject_waited_ms.is_some(),
        "the subject's honest wait"
    );
    assert!(
        drain.subject_floor_ms.is_some(),
        "the observed-arrival floor"
    );
    assert_eq!(
        drain.twin_completed,
        Some(false),
        "the never-idle twin recorded the burned bound"
    );
    assert!(drain.twin_waited_ms.is_some(), "the twin's honest wait");

    let readiness = &evidence.readiness;
    assert_eq!(
        readiness.hold_proven,
        Some(true),
        "the healthy leg OBSERVED the declared --http-ready-after hold (A2)"
    );
    assert!(readiness.mid_hold_saw_starting.is_some());
    assert!(readiness.mid_hold_saw_running.is_some());
    assert_eq!(
        readiness.ready_edge_adapter_ready,
        Some(true),
        "the adapter-ready edge receipt"
    );
    assert!(
        readiness.timeout_waited_ms.is_some(),
        "the twin's honest wait"
    );

    let liveness = &evidence.liveness;
    assert!(liveness.healthy_held_ms.is_some(), "the healthy hold");
    assert!(liveness.failing_streak_ms.is_some(), "the twin's streak");

    let exit_classes = &evidence.exit_classes;
    assert_eq!(
        exit_classes.relaunch_counter_unchanged,
        Some(true),
        "the exit-75 counter receipt"
    );
    assert_eq!(
        exit_classes.charger_counter_before,
        Some(0),
        "the charger leg's counter STARTED at zero (A4)"
    );
    assert!(
        matches!(exit_classes.charger_counter_after, Some(n) if n >= 1),
        "the unclassified exit CHARGED the counter (0→1)"
    );
    assert_eq!(exit_classes.clean_settled, Some(true), "the clean receipt");

    let pause = &evidence.pause;
    assert!(
        matches!(&pause.pause_command, Some(cmd) if cmd.contains("pause.sentinel")),
        "the cooperative pause cause's rendered command: {:?}",
        pause.pause_command
    );
    assert!(
        matches!(&pause.resume_command, Some(cmd) if cmd.contains("resume.sentinel")),
        "the cooperative resume cause's rendered command: {:?}",
        pause.resume_command
    );

    // C5: the subject's own work log, read through the engine's PUBLIC path
    // layout — the agent demonstrably worked (work lines), not merely moved
    // through lifecycle states.
    let work_log = EnginePaths::new(Some(root.to_path_buf()))
        .expect("the caller-owned root resolves")
        .agent_home(&InstanceName::new("tck-subject").expect("a valid name"))
        .join(GATEWAY_WORK_LOG);
    let log = std::fs::read_to_string(&work_log)
        .unwrap_or_else(|e| panic!("read the subject's work log {}: {e}", work_log.display()));
    assert!(
        log.lines().any(|line| line.starts_with("work ")),
        "the subject's --watch-sentinel thread appended work lines (got: {log:?})"
    );
}
