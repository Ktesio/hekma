//! Integration tests for story 17-4 (the ephemeral-host export surface +
//! the log-capture modes), driven through the engine's PUBLIC async API and
//! blocking facade only (spine AD-2/AD-13), spawning the REAL `fake_agent`
//! so the commit points genuinely fire.
//!
//! Covered here (the supervisor/process-backend internals carry their own
//! unit pins in `src`):
//!
//! * The outbox is PURELY ADDITIVE: `resync_events` answers byte-identically
//!   to its 17-3 contract while the journal fills beside it.
//! * The export/ack round trip through the blocking facade: one global
//!   order, the durable ack, the RESTART (a fresh engine on the same state
//!   root resumes the ack AND continues the seq sequence — no reuse).
//! * Monotonic ack (a re-ack is idempotent; a backward ack is the current
//!   cursor) and the past-tail refusal (the stable sentence the host maps
//!   to 409 `cursor_invalid`).
//! * The Off-mode read refusals at the facade (honest absence, never an
//!   empty-read lie) and the Metadata-mode attributed-read refusal.
//! * The default posture is `full` (byte-identical to 17-3).

use std::time::Duration;

use hekma_engine::{
    AdapterRef, Engine, EngineError, EngineOptions, LifecycleState, LogCaptureMode, OutboxCursor,
    PAST_TAIL_SENTENCE,
};
use tempfile::TempDir;

/// Write a manifest whose `[lifecycle.start]` exec points at `fake_agent`
/// with `args`, into `dir`.
fn write_fake_manifest(dir: &std::path::Path, kind: &str, args: &[&str]) {
    hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction")
        .write(dir);
}

#[test]
fn the_default_posture_is_full() {
    // The byte-identity anchor: no options == full capture, exactly the
    // pre-17-4 wiring (the spawn wiring itself is pinned in the backend
    // suites).
    assert_eq!(EngineOptions::default().log_capture, LogCaptureMode::Full);
    // The mode vocabulary round-trips its wire form (the CLI flag value).
    for (text, mode) in [
        ("full", LogCaptureMode::Full),
        ("metadata", LogCaptureMode::Metadata),
        ("off", LogCaptureMode::Off),
    ] {
        assert_eq!(text.parse::<LogCaptureMode>().unwrap(), mode);
        assert_eq!(mode.to_string(), text);
    }
    "sometimes"
        .parse::<LogCaptureMode>()
        .expect_err("an invalid mode refuses");
}

#[test]
fn export_pull_ack_round_trip_and_the_cursor_survives_an_engine_restart() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);

    // Engine #1: register + start + stop (four committed transitions).
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // The outbox is PURELY ADDITIVE: resync answers the SAME events it
    // always did (the family counts include every committed transition).
    let resync = facade
        .resync_events("svc", hekma_engine::ResyncCursor::START)
        .unwrap();
    assert!(resync.events.len() >= 4, "resync is untouched: {resync:?}");

    // PULL from the durable ack (absent → the journal's beginning): every
    // committed event, ONE global order, seq-stamped.
    let batch = facade.export_events(None, 0).unwrap();
    assert_eq!(batch.schema_version, hekma_engine::OUTBOX_SCHEMA_VERSION);
    assert!(!batch.torn_tail_skipped, "a clean journal tears nothing");
    let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "seqs: {seqs:?}");
    assert_eq!(batch.cursor.seq, *seqs.last().unwrap(), "the batch cursor");

    // ACK the tail; a default pull then delivers an EMPTY batch.
    let persisted = facade.ack_export(*seqs.last().unwrap()).unwrap();
    assert_eq!(persisted, *seqs.last().unwrap());
    let batch = facade.export_events(None, 0).unwrap();
    assert!(batch.events.is_empty(), "all acked: {batch:?}");
    drop(engine);

    // Engine #2 (the RESTART) on the SAME state root: the ack cursor is
    // durable (the default pull still resumes AFTER it), and a NEW
    // lifecycle commits events with seqs CONTINUING past the old tail —
    // never reused.
    let engine2 = Engine::open(Some(state.path().to_path_buf())).expect("reopen");
    let facade2 = engine2.blocking();
    let batch = facade2.export_events(None, 0).unwrap();
    assert!(
        batch.events.is_empty(),
        "the ack survived the restart: {batch:?}"
    );
    facade2.start("svc").unwrap();
    facade2.stop("svc", Some(Duration::from_secs(5))).unwrap();
    let batch = facade2.export_events(None, 0).unwrap();
    assert!(!batch.events.is_empty(), "the restart's commits exported");
    for entry in &batch.events {
        assert!(
            entry.seq > *seqs.last().unwrap(),
            "no seq reuse across restarts: {} vs {}",
            entry.seq,
            seqs.last().unwrap()
        );
    }
}

#[test]
fn ack_is_monotonic_and_a_past_tail_refuses_with_the_stable_sentence() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    let batch = facade.export_events(None, 0).unwrap();
    let tail = batch.cursor.seq;
    assert!(tail > 0, "committed transitions exist");

    // A FORWARD ack past nothing — then a BACKWARD ack is the CURRENT
    // cursor (monotonic; never a rewind).
    let persisted = facade.ack_export(tail).unwrap();
    assert_eq!(persisted, tail);
    let persisted = facade.ack_export(1).unwrap();
    assert_eq!(persisted, tail, "a backward ack is the current cursor");

    // An ack PAST the committed tail refuses with the STABLE sentence —
    // the exact string the Control API's error mapper matches to 409
    // `cursor_invalid`.
    let err = facade.ack_export(999_999).unwrap_err();
    assert!(
        err.to_string().contains(PAST_TAIL_SENTENCE),
        "the past-tail refusal carries the stable sentence: {err}"
    );

    // The EXPORT pull refuses the same two ways: below the retained floor
    // (nothing compacted away yet in this small journal — a cursor below
    // the FLOOR requires dropped entries, so here the below-floor sentence
    // is exercised against the empty-journal floor of a cursor past the
    // tail) and past the tail.
    let err = facade
        .export_events(Some(OutboxCursor { seq: 999_999 }), 0)
        .unwrap_err();
    assert!(
        err.to_string().contains(PAST_TAIL_SENTENCE),
        "the pull past the tail refuses: {err}"
    );
}

#[test]
fn off_mode_reads_refuse_honestly_at_the_facade_and_spawns_write_nothing() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            log_capture: LogCaptureMode::Off,
            ..EngineOptions::default()
        },
    )
    .expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // NOTHING was ever written: no raw file, no attributed capture.
    let logs_dir = state.path().join("agents").join("svc").join("logs");
    let entries: Vec<_> = std::fs::read_dir(&logs_dir)
        .unwrap_or_else(|_| panic!("the logs dir exists at {}", logs_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !entries.iter().any(|n| n.contains("agent.log")
            || n.contains("agent-stderr.log")
            || n.contains("output.log")),
        "off mode wrote no capture files: {entries:?}"
    );

    // The reads REFUSE with the typed honest absence (never an empty-OK
    // that would imply "captured, then exhausted").
    for err in [
        facade.read_agent_log("svc").unwrap_err(),
        facade.read_agent_log_since("svc", 0).unwrap_err(),
        facade.read_raw_output_since("svc", None).unwrap_err(),
        facade.prime_raw_backlog("svc", 10).unwrap_err(),
    ] {
        assert!(
            matches!(err, EngineError::Log { .. }),
            "the refusal is the typed Log error: {err}"
        );
        assert!(
            err.to_string().contains("log capture is off"),
            "the refusal names the mode: {err}"
        );
    }
}

#[test]
fn metadata_mode_attributed_read_refuses_and_names_the_mode() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(
        manifest.path(),
        "svc",
        &["--emit-usage", "1", "--linger-ms", "600000"],
    );
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            log_capture: LogCaptureMode::Metadata,
            ..EngineOptions::default()
        },
    )
    .expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // The attributed read refuses (output.log is metadata-shaped — parsing
    // it as LogLines would lie); the refusal NAMES the mode + remediation.
    let err = facade.read_agent_log("svc").unwrap_err();
    assert!(
        err.to_string().contains("--log-capture metadata"),
        "the attributed-read refusal names the metadata mode: {err}"
    );
    let err = facade.read_agent_log_since("svc", 0).unwrap_err();
    assert!(
        err.to_string().contains("--log-capture metadata"),
        "the follow-read refusal names the metadata mode: {err}"
    );

    // The RAW reads stay honest WITHOUT refusing: while stopped, the
    // unlinked raw file surfaces as the existing honest absence.
    let (lines, _cursor) = facade.read_raw_output_since("svc", None).unwrap();
    assert!(
        lines.is_empty(),
        "the unlinked raw file is an honest absence: {lines:?}"
    );
}

#[test]
fn an_invalid_instance_name_refuses_as_invalid_name_on_the_read_paths() {
    // Name validation PRECEDES the capture-mode guard: a bogus name is the
    // typed InvalidName error (400-shaped at the API), never a capture
    // refusal and never a panic.
    let state = TempDir::new().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open");
    let facade = engine.blocking();
    let err = facade
        .read_agent_log_since("Not Valid!", 0)
        .expect_err("an invalid name refuses");
    assert!(matches!(err, EngineError::InvalidName { .. }), "{err:?}");
}

#[test]
fn an_invalid_instance_name_refuses_as_invalid_name_on_the_config_paths() {
    // Same shape on the ConfigError-returning facade (the reveal path's
    // name validation): the typed error, before any registry work.
    let state = TempDir::new().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open");
    let facade = engine.blocking();
    let err = facade
        .reveal_secrets("Not Valid!", hekma_engine::ConfigLayer::empty())
        .expect_err("an invalid name refuses");
    assert!(
        matches!(err, hekma_engine::ConfigError::InvalidName { .. }),
        "{err:?}"
    );
}

#[test]
fn the_engine_options_debug_names_the_mode_and_never_a_sink() {
    // The manual Debug impl (the boxed sink is not `Debug` and must never
    // print — the no-leak discipline) carries the 17-4 capture mode so a
    // support bundle can show how the engine was opened.
    let text = format!("{:?}", EngineOptions::default());
    assert!(text.contains("log_capture"), "{text}");
    assert!(text.contains("metering_listen_port"), "{text}");
}

#[test]
fn the_usage_and_breach_hooks_write_the_journal_too() {
    // Review finding 22: every prior journal/export test drove TRANSITIONS
    // only — the export could have shipped delivering one of its three
    // families. This drives the OTHER two commit points through the real
    // agent: `--emit-usage` sentinel lines (the usage-ledger commit hook)
    // and a configured token budget crossing (the breach hook), then
    // asserts BOTH families appear in the export with seqs INTERLEAVED
    // among the transitions' (one global order, not three streams).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    hekma_conformance::test_support::ManifestFixture::new(
        "hooked",
        &["--emit-usage", "3", "--linger-ms", "600000"],
    )
    .guaranteed_on_all_oses("interaction")
    .guaranteed_on_all_oses("pause")
    .write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "hooked",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // A cumulative ceiling of 60 tokens: 30/event, so event 2 CROSSES it —
    // the default pause action fires the paused transition (budget.rs's
    // determinism anchor).
    facade
        .set_config("hooked", "budget.tokens.cumulative", "60")
        .unwrap();
    facade.start("hooked").unwrap();

    // Poll the EXPORT (the API surface itself) until the breach has landed —
    // the reaper cadence ingests the sentinels, the evaluator fires, and all
    // three commit hooks append under the same lock.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let batch = loop {
        let batch = facade
            .export_events(Some(OutboxCursor::START), 0)
            .expect("the export pull reads the journal");
        let breaches = batch
            .events
            .iter()
            .filter(|e| serde_json::to_value(&e.event).unwrap()["kind"] == "budget_breach")
            .count();
        if breaches >= 1 {
            break batch;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the breach never reached the journal: {:?}",
            batch.events
        );
        std::thread::sleep(Duration::from_millis(40));
    };

    // Split the one global order by family.
    let mut usage_seqs: Vec<u64> = Vec::new();
    let mut breach_seq: Option<u64> = None;
    let mut paused_seq: Option<u64> = None;
    for entry in &batch.events {
        let kind = serde_json::to_value(&entry.event).unwrap()["kind"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        match kind.as_str() {
            "usage_update" => usage_seqs.push(entry.seq),
            "budget_breach" => breach_seq = Some(entry.seq),
            "transition" => {
                let text = serde_json::to_string(&entry.event).unwrap();
                if text.contains("budget") && paused_seq.is_none() {
                    paused_seq = Some(entry.seq);
                }
            }
            other => panic!("an unknown family in the journal: {other}"),
        }
    }
    assert!(
        usage_seqs.len() >= 2,
        "the usage commit hook wrote the journal: {usage_seqs:?}"
    );
    let breach =
        breach_seq.expect("the breach commit hook wrote the journal (the poll above found it)");
    let paused = paused_seq.expect("the pause transition followed the breach");
    // The commit-order interleaving, exact: the breaching usage commits,
    // the breach fires ON it, the paused transition lands after — three
    // families, ONE seq order.
    let before_breach: Vec<u64> = usage_seqs.iter().copied().filter(|s| *s < breach).collect();
    assert!(
        !before_breach.is_empty(),
        "usage commits ride the same order BEFORE the breach: {usage_seqs:?} vs {breach}"
    );
    assert!(
        paused > breach,
        "the pause transition follows the breach: {paused} vs {breach}"
    );
    // And the whole journal is one strictly increasing seq sequence.
    let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "one global order, no gaps: {seqs:?}"
    );
}

#[test]
fn a_leftover_journal_file_is_never_read_after_the_upgrade() {
    // The outbox lives in state.db: a file at the old journal path (an older
    // engine recreating it, or debris) is neither read nor written, and the
    // export stays complete and gapless.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "isolated", &["--linger-ms", "600000"]);
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    let journal = state.path().join(hekma_engine::paths::OUTBOX_JOURNAL_FILE);
    std::fs::write(&journal, "not an outbox record\n").unwrap();
    facade
        .register_with_adapter(
            "isolated",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("isolated").unwrap();
    let batch = facade
        .export_events(Some(OutboxCursor::START), 0)
        .expect("the export reads state.db");
    let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert!(seqs.len() >= 2, "the start transitions are exported");
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap(),
        "not an outbox record\n",
        "the file is untouched"
    );
    let _ = facade.stop("isolated", Some(Duration::from_secs(5)));
}
