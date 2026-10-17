//! Integration tests for story 15-6: memory attach/detach × start atomicity —
//! the durable-store transactions closing the CROSS-PROCESS windows, driven
//! through the engine's PUBLIC API + blocking facade (spine AD-2).
//!
//! The real CLI shape is one `Engine` per command sharing a state dir, and
//! the only state two CLI processes share is the SQLite file. The test
//! shapes split accordingly (review round 1, doc scoping):
//!
//! * The DETERMINISTIC windows pin the STORE SEAM: the engine-side racer is
//!   one `Engine` (or `Engine::start`) whose transition transaction is PARKED
//!   at the store by a held write lock, while the opposite racer is a RAW
//!   CONNECTION on the same database committing exactly the row/state
//!   mutation a racing `hekma agent memory attach` / `detach` / `start`
//!   would commit. What is under test is the transaction's guard — a
//!   raw-connection racer is the honest minimal second process for that.
//! * The UNSYNCHRONIZED race drives TWO real `Engine`s over one root — the
//!   full one-Engine-per-command shape with no synchronization, pinning the
//!   outcome dichotomy.
//!
//! What is pinned here (the spec's I/O matrix, row by row):
//!
//! * **Window 1 (attach in start's read→transition window)** — the `→
//!   starting` transition transaction sees the attach row, disagrees with the
//!   start's pre-check read, refuses TYPED + retryable, and rolls back: no
//!   `starting` row, no launch, the other writer's commit stands. (A
//!   scheduler-stalled start that instead surfaces the typed Busy error
//!   within the bounded window is the matrix's other legal outcome and is
//!   tolerated.)
//! * **Detach mirror** — same dichotomy for a mid-start detach, plus the
//!   sink-emptiness pin (a rejected start emits NO diagnostic — not even the
//!   DC-10 delivery notice the attach would have earned a launched start).
//! * **Window 2 (attach after the transition txn)** — the attach guard
//!   refuses side-effect-free on the non-terminal instance (no hot-swap).
//! * **Window 3 (transition inside the attach guard→write gap)** — the
//!   attach's guard re-evaluates UNDER the write lock: no backing row lands
//!   on a non-terminal instance, row byte-unchanged.
//! * **Two concurrent starts** — the losing `Engine::start` refuses with the
//!   typed `StateConflict` and rolls back; the winner's truth stands.
//! * **Busy bound** — two concurrent writers degrade to the `busy_timeout`
//!   bounded wait and then the TYPED busy error, never a raw SQLITE_BUSY.
//! * **The unsynchronized two-Engine race** — attach and start fired
//!   together; the ONLY accepted outcomes are the matrix's dichotomy: start
//!   launches WITH the attached dir (attach committed before start's
//!   authoritative read), start rejects typed (attach committed inside the
//!   window), or the attach LOSES to the transition (its typed hot-swap
//!   refusal, no row, and the start launches clean) — never a launched agent
//!   contradicting the row.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::ports::StoreError;
use hekma_engine::{Engine, EngineError, LifecycleState, MemoryBackingKind, RegistryError};
use rusqlite::Connection;
use tempfile::TempDir;

use std::sync::{Arc, Mutex};

/// The bounded poll for a COMMITTED in-window artifact (the effective-config
/// snapshot file start writes BEFORE the transition transaction — its
/// existence proves the start has passed its pre-check reads and is parked at
/// the store transaction). Same committed-artifact polling discipline as the
/// sibling suites (no fixed sleeps to await state).
const WINDOW_POLL_BUDGET: Duration = Duration::from_secs(10);

/// Open the engine with a CAPTURED diagnostic sink (the story-10-2 host
/// shape) and return the engine plus the shared capture — the
/// sink-emptiness pins read the capture to prove a rejected start emitted
/// NOTHING (not even the DC-10 delivery notice).
fn open_with_sink(base: &TempDir) -> (hekma_engine::Engine, Arc<Mutex<Vec<u8>>>) {
    /// A `Write` adapter delegating into the shared capture buffer (the
    /// `diagnostic_sink.rs` harness shape).
    struct SharedCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink: hekma_engine::DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(
        Arc::clone(&captured),
    )) as Box<_>));
    let engine = hekma_engine::Engine::open_with_diagnostics(Some(base.path().to_path_buf()), sink)
        .expect("open engine with the captured sink");
    (engine, captured)
}

fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// A launchable, lingering fake_agent manifest (the good branch of the race
/// really launches; the rejected branches must not).
fn write_lingering_manifest(dir: &Path, name: &str) -> PathBuf {
    ManifestFixture::fake_agent(name, &["--linger-ms", "600000"]).write(dir)
}

/// A raw connection on the SAME state database — the "second process" side of
/// the deterministic windows (the only state two CLI Engines share).
fn raw_conn(state: &TempDir) -> Connection {
    Connection::open(
        hekma_engine::paths::EnginePaths::new(Some(state.path().to_path_buf()))
            .expect("state paths")
            .state_db(),
    )
    .expect("open the raw racer connection")
}

/// The managed memory dir path (engine path authority).
fn memory_dir(state: &TempDir, name: &str) -> PathBuf {
    hekma_engine::paths::EnginePaths::new(Some(state.path().to_path_buf()))
        .expect("state paths")
        .agent_memory_dir(&hekma_engine::InstanceName::new(name).unwrap())
}

/// The instance's persisted state (wire form), read straight from the DB.
fn persisted_state(conn: &Connection, name: &str) -> String {
    conn.query_row(
        "SELECT state FROM agent_instances WHERE name = ?1",
        [name],
        |r| r.get(0),
    )
    .expect("instance row exists")
}

/// The backing row's kind (or None), read straight from the DB.
fn persisted_backing_kind(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row(
        "SELECT kind FROM agent_memory_backing WHERE instance_id = \
         (SELECT id FROM agent_instances WHERE name = ?1)",
        [name],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// The write-ahead spawn-record rows an instance has, read straight from the
/// DB — the REAL nothing-launched check (only a spawned-and-recorded process
/// writes one; a rejected start writes none).
fn persisted_spawn_record_count(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM agent_runtime WHERE instance_id = \
         (SELECT id FROM agent_instances WHERE name = ?1)",
        [name],
        |r| r.get(0),
    )
    .expect("spawn-record count")
}

/// Commit the attachment row AS THE RACING PROCESS would (the durable fact
/// `hekma agent memory attach` persists), on the raw connection.
fn commit_attach_as_racer(conn: &Connection, name: &str, kind: &str) {
    let id: i64 = conn
        .query_row(
            "SELECT id FROM agent_instances WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .expect("instance row exists");
    conn.execute(
        "INSERT INTO agent_memory_backing (instance_id, kind, attached_at) \
         VALUES (?1, ?2, '2026-10-17T00:00:00Z') \
         ON CONFLICT(instance_id) DO UPDATE SET kind = excluded.kind",
        rusqlite::params![id, kind],
    )
    .expect("commit the racing attach row");
}

/// Block until `path` exists — the in-window marker (see WINDOW_POLL_BUDGET).
fn wait_for_in_window_marker(path: &Path, what: &str) {
    let deadline = Instant::now() + WINDOW_POLL_BUDGET;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "the start never reached its pre-transition snapshot write ({what}): {path:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A best-effort cleanup stop so a flaky failure never leaks a lingering
/// fake_agent into the rest of the suite.
fn stop_best_effort(engine: &Engine, name: &str) {
    let _ = engine.blocking().stop(name, Some(Duration::from_secs(5)));
}

// ---------------------------------------------------------------------------
// Window 1: attach lands inside start's read→transition window
// ---------------------------------------------------------------------------

#[test]
fn an_attach_landing_inside_starts_window_rejects_the_start_typed_and_rolls_back() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "svc");

    // ONE engine over the root; the racing side is the raw connection on the
    // shared store (the deterministic windows pin the STORE seam — see the
    // module docs).
    let engine_a = open(&state);
    let facade_a = engine_a.blocking();
    let registered = facade_a
        .register_with_adapter("svc", &hekma_engine::AdapterRef::Manifest(manifest_path))
        .unwrap();
    let home = PathBuf::from(&registered.agent_home);
    let snapshot_marker = home.join("effective-config.json");

    // Hold the write lock from the racer BEFORE start runs, so start's
    // transition transaction PARKS mid-window.
    let racer = raw_conn(&state);
    racer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let start_result = std::thread::scope(|s| {
        let handle = s.spawn(move || facade_a.start("svc"));
        // The snapshot marker proves the start passed its pre-check backing
        // read (None) and is parked at the transition transaction.
        wait_for_in_window_marker(&snapshot_marker, "attach race");
        // The racing attach COMMITS INSIDE THE WINDOW, at the shared store.
        commit_attach_as_racer(&racer, "svc", "filesystem");
        racer.execute_batch("COMMIT").unwrap();
        handle.join().unwrap()
    });

    // THE PIN: typed + retryable, naming both sides — never a launch. (E8:
    // a start whose BEGIN IMMEDIATE instead waited out the whole bounded
    // busy window under a stalled scheduler surfaces the typed Busy error —
    // the matrix's other legal concurrent-writers outcome — and every
    // post-condition below holds identically.)
    match &start_result {
        Err(EngineError::MemoryBackingChangedUnderStart {
            precheck, snapshot, ..
        }) => {
            assert_eq!(precheck, "none", "the pre-check read nothing");
            assert_eq!(
                snapshot, "kind 'filesystem' attached at 2026-10-17T00:00:00Z",
                "the txn snapshot names the racing attach"
            );
        }
        Err(EngineError::Store(StoreError::Busy { .. })) => {}
        Ok(_) => panic!("start launched while the backing changed under it"),
        Err(other) => panic!("expected the typed mismatch error, got {other:?}"),
    }

    // The transition ROLLED BACK: state unchanged, no transition event
    // recorded, nothing launched.
    let conn = raw_conn(&state);
    assert_eq!(persisted_state(&conn, "svc"), "registered");
    assert!(
        engine_a
            .blocking()
            .transition_events("svc")
            .unwrap()
            .is_empty(),
        "a rolled-back start records no transition event"
    );
    assert_eq!(
        persisted_spawn_record_count(&conn, "svc"),
        0,
        "a rejected start writes no write-ahead spawn record — nothing launched"
    );
    // The other writer's commit STANDS (start must not undo the fleet truth).
    assert_eq!(
        persisted_backing_kind(&conn, "svc").as_deref(),
        Some("filesystem")
    );
    // And nothing launched against it: no managed dir was ever created by
    // the rejected start (the pre-check saw None).
    assert!(
        !memory_dir(&state, "svc").exists(),
        "the rejected start must not launch (no self-heal dir)"
    );
}

// ---------------------------------------------------------------------------
// The detach mirror (same window, opposite mutation) + the sink-emptiness pin
// ---------------------------------------------------------------------------

#[test]
fn a_detach_landing_inside_starts_window_rejects_the_start_typed_and_rolls_back() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "svc");

    // Opened WITH a captured diagnostic sink: the vg pin — a rejected start
    // must emit NOTHING, not even the DC-10 delivery notice this unmapped
    // manifest would have earned a launched start (a notice for a launch
    // that never happened would be a lie).
    let (engine_a, sink) = open_with_sink(&state);
    let facade_a = engine_a.blocking();
    let registered = facade_a
        .register_with_adapter("svc", &hekma_engine::AdapterRef::Manifest(manifest_path))
        .unwrap();
    let home = PathBuf::from(&registered.agent_home);
    let snapshot_marker = home.join("effective-config.json");

    // The attachment exists BEFORE the start; the racing detach clears its
    // row INSIDE the window.
    facade_a
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();

    let racer = raw_conn(&state);
    racer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let start_result = std::thread::scope(|s| {
        let handle = s.spawn(move || facade_a.start("svc"));
        wait_for_in_window_marker(&snapshot_marker, "detach race");
        // The racing detach: the backing row CLEARS inside the window.
        let id: i64 = racer
            .query_row(
                "SELECT id FROM agent_instances WHERE name = 'svc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        racer
            .execute(
                "DELETE FROM agent_memory_backing WHERE instance_id = ?1",
                [id],
            )
            .unwrap();
        racer.execute_batch("COMMIT").unwrap();
        handle.join().unwrap()
    });

    match &start_result {
        Err(EngineError::MemoryBackingChangedUnderStart {
            precheck, snapshot, ..
        }) => {
            assert!(
                precheck.starts_with("kind 'filesystem'"),
                "the pre-check saw the attachment: {precheck}"
            );
            assert_eq!(snapshot, "none", "the txn snapshot saw the clear");
        }
        // E8: the scheduler-stall Busy alternative (see window 1).
        Err(EngineError::Store(StoreError::Busy { .. })) => {}
        Ok(_) => panic!("start launched while the backing was detached under it"),
        Err(other) => panic!("expected the typed mismatch error, got {other:?}"),
    }

    let conn = raw_conn(&state);
    assert_eq!(persisted_state(&conn, "svc"), "registered");
    assert_eq!(persisted_backing_kind(&conn, "svc"), None);
    assert!(
        engine_a
            .blocking()
            .transition_events("svc")
            .unwrap()
            .is_empty(),
        "the rolled-back start records no transition"
    );
    assert_eq!(
        persisted_spawn_record_count(&conn, "svc"),
        0,
        "nothing launched: no write-ahead spawn record exists for the rejected start"
    );
    // The sink-emptiness pin (vg): the rejection emitted NO diagnostic —
    // the empty capture is the proof (the delivery notice lives post-
    // transition and derives from the snapshot that never agreed).
    assert!(
        sink.lock().unwrap().is_empty(),
        "a rejected start must emit no diagnostic (not even the DC-10 notice): {:?}",
        String::from_utf8_lossy(&sink.lock().unwrap())
    );
}

// ---------------------------------------------------------------------------
// Window 2: attach commits AFTER the transition txn
// ---------------------------------------------------------------------------

#[test]
fn an_attach_committing_after_the_starting_transition_refuses_side_effect_free() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "svc");

    let engine_a = open(&state);
    let facade_a = engine_a.blocking();
    facade_a
        .register_with_adapter("svc", &hekma_engine::AdapterRef::Manifest(manifest_path))
        .unwrap();

    // The start's transition txn commits FIRST (no racer): the instance runs.
    let started = facade_a.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The racing attach lands AFTER the transition: the guard refuses with
    // the non-terminal refusal — no hot-swap onto a live instance, row
    // unchanged (the forbidden row would be the hot-swap).
    let err = facade_a
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap_err();
    match &err {
        RegistryError::MemoryBackingHotSwap { name, state } => {
            assert_eq!(name, "svc");
            assert_eq!(state, "running");
        }
        other => panic!("expected MemoryBackingHotSwap, got {other:?}"),
    }
    let conn = raw_conn(&state);
    assert_eq!(persisted_backing_kind(&conn, "svc"), None);
    stop_best_effort(&engine_a, "svc");
}

// ---------------------------------------------------------------------------
// Window 3: a lifecycle transition lands inside the attach guard→write gap
// ---------------------------------------------------------------------------

#[test]
fn a_transition_landing_inside_the_attach_guard_gap_refuses_side_effect_free() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "svc");

    let engine_b = open(&state);
    let facade_b = engine_b.blocking();
    facade_b
        .register_with_adapter("svc", &hekma_engine::AdapterRef::Manifest(manifest_path))
        .unwrap();

    // The racer holds the write lock; the attach's PRE-CHECK guard reads the
    // terminal `registered` state through (WAL readers never block) and then
    // PARKS at its transaction.
    let racer = raw_conn(&state);
    racer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let attach_result = std::thread::scope(|s| {
        let handle = s.spawn(move || facade_b.attach_memory("svc", MemoryBackingKind::Filesystem));
        // Give the attach a bounded moment to reach its parked transaction,
        // then transition the instance as the racing `start` would. There is
        // no observable pre-commit artifact to poll (the attach's reads are
        // invisible), but BOTH orders converge on the pinned refusal: if the
        // attach's pre-check ran before the UPDATE, its in-txn guard re-reads
        // `starting` under the write lock and refuses; if it ran after, the
        // pre-check itself refuses. Identical surfacing either way.
        std::thread::sleep(Duration::from_millis(200));
        racer
            .execute(
                "UPDATE agent_instances SET state = 'starting', updated_at = \
                 '2026-10-17T00:00:00Z' WHERE name = 'svc'",
                [],
            )
            .unwrap();
        racer.execute_batch("COMMIT").unwrap();
        handle.join().unwrap()
    });

    // THE PIN: the guard re-evaluated under the write lock and refused —
    // typed non-terminal refusal, the backing row never lands.
    match attach_result {
        Err(RegistryError::MemoryBackingHotSwap { name, state }) => {
            assert_eq!(name, "svc");
            assert_eq!(state, "starting");
        }
        Ok(_) => panic!("a backing row landed on a transitioning instance"),
        Err(other) => panic!("expected MemoryBackingHotSwap, got {other:?}"),
    }
    let conn = raw_conn(&state);
    assert_eq!(persisted_state(&conn, "svc"), "starting");
    assert_eq!(persisted_backing_kind(&conn, "svc"), None, "no row landed");
}

// ---------------------------------------------------------------------------
// Two concurrent starts: the loser refuses with the typed StateConflict
// ---------------------------------------------------------------------------

#[test]
fn the_losing_side_of_two_concurrent_starts_refuses_typed_state_conflict() {
    // The engine-level StateConflict pin (review round 1): `Engine::start`
    // itself — not just the raw store seam — refuses typed when another
    // command's `→ starting` transition lands inside this start's
    // read→transition window. The racing "second start" is the raw
    // connection committing exactly the transition a concurrent
    // `hekma agent start` would commit, while this start's transaction is
    // parked at the store.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "svc");

    let engine_a = open(&state);
    let facade_a = engine_a.blocking();
    let registered = facade_a
        .register_with_adapter("svc", &hekma_engine::AdapterRef::Manifest(manifest_path))
        .unwrap();
    let home = PathBuf::from(&registered.agent_home);
    let snapshot_marker = home.join("effective-config.json");

    let racer = raw_conn(&state);
    racer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let start_result = std::thread::scope(|s| {
        let handle = s.spawn(move || facade_a.start("svc"));
        wait_for_in_window_marker(&snapshot_marker, "two-starts race");
        // The racing start's transition commits INSIDE the window.
        racer
            .execute(
                "UPDATE agent_instances SET state = 'starting', updated_at = \
                 '2026-10-17T00:00:00Z' WHERE name = 'svc'",
                [],
            )
            .unwrap();
        racer.execute_batch("COMMIT").unwrap();
        handle.join().unwrap()
    });

    // THE PIN: the conditional transition refused typed — this start's
    // `registered` was already superseded by the other command's `starting`.
    match &start_result {
        Err(EngineError::Store(StoreError::StateConflict {
            name,
            expected,
            found,
        })) => {
            assert_eq!(name, "svc");
            assert_eq!(expected, "registered", "this start read `registered`");
            assert_eq!(
                found, "starting",
                "the txn saw the other start's transition"
            );
        }
        Err(EngineError::Store(StoreError::Busy { .. })) => {
            // E8: the scheduler-stall Busy alternative (see window 1).
        }
        Ok(_) => panic!("a second start launched against a raced transition"),
        Err(other) => panic!("expected the typed StateConflict, got {other:?}"),
    }

    // The loser's rollback must NOT undo the winner's truth: the state is
    // the racing start's `starting`, nothing was launched by the loser, and
    // no transition event was recorded for the refused side.
    let conn = raw_conn(&state);
    assert_eq!(
        persisted_state(&conn, "svc"),
        "starting",
        "the other command's transition stands (the rollback is local)"
    );
    assert!(
        engine_a
            .blocking()
            .transition_events("svc")
            .unwrap()
            .is_empty(),
        "the refused start records no transition event"
    );
    assert_eq!(
        persisted_spawn_record_count(&conn, "svc"),
        0,
        "the refused start wrote no spawn record — nothing launched"
    );
}

// ---------------------------------------------------------------------------
// The unsynchronized two-Engine race: the dichotomy invariant
// ---------------------------------------------------------------------------

#[test]
fn two_engines_racing_attach_against_start_only_ever_launch_with_the_row() {
    // No synchronization: whichever writer wins the write lock, the ONLY
    // accepted outcomes are the matrix's dichotomy — start launches WITH the
    // attached dir (attach committed before start's authoritative read), or
    // start rejects typed (attach committed inside the window), or the
    // attach LOSES to the transition (its typed hot-swap refusal, no row,
    // and the start launches clean — its snapshot honestly saw none). A
    // launched agent contradicting the row is the forbidden outcome this
    // test exists to catch.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "race");

    let engine_a = open(&state);
    let engine_b = open(&state);
    engine_a
        .blocking()
        .register_with_adapter("race", &hekma_engine::AdapterRef::Manifest(manifest_path))
        .unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let barrier_a = std::sync::Arc::clone(&barrier);
    let barrier_b = std::sync::Arc::clone(&barrier);

    let (started, attached, engine_a) = std::thread::scope(|s| {
        let a = s.spawn(move || {
            barrier_a.wait();
            let started = engine_a.blocking().start("race");
            (started, engine_a)
        });
        let b = s.spawn(move || {
            barrier_b.wait();
            engine_b
                .blocking()
                .attach_memory("race", MemoryBackingKind::Filesystem)
        });
        let (started, engine_a) = a.join().unwrap();
        let attached = b.join().unwrap();
        (started, attached, engine_a)
    });

    let conn = raw_conn(&state);
    let row_kind = persisted_backing_kind(&conn, "race");
    match (&attached, &started) {
        // The attach WON the write lock before start's authoritative read:
        // a launch must agree with the committed row.
        (Ok(_), Ok(instance)) => {
            assert_eq!(instance.state, LifecycleState::Running);
            assert_eq!(
                row_kind.as_deref(),
                Some("filesystem"),
                "launched WITHOUT the dir the row would deny"
            );
            assert!(
                memory_dir(&state, "race").exists(),
                "the launched start self-healed the managed dir"
            );
        }
        // The attach committed INSIDE start's window: the typed mid-start
        // rejection — nothing launched, state rolled back, the racing
        // attach's truth stands.
        (Ok(_), Err(EngineError::MemoryBackingChangedUnderStart { .. })) => {
            assert_eq!(persisted_state(&conn, "race"), "registered");
            assert_eq!(row_kind.as_deref(), Some("filesystem"));
        }
        // The attach LOST the race to the transition (E7): its guard
        // re-read a non-terminal state under the write lock and refused —
        // no row landed, and the start launches CLEAN against the honest
        // none-snapshot.
        (Err(RegistryError::MemoryBackingHotSwap { .. }), Ok(instance)) => {
            assert_eq!(instance.state, LifecycleState::Running);
            assert_eq!(
                row_kind, None,
                "the losing attach must leave no row for the launched start to contradict"
            );
        }
        // The contradictory pair is unreachable: a hot-swapping refusal
        // means the transition had committed, so the start's snapshot
        // matched its pre-check (nothing changed under it).
        (
            Err(RegistryError::MemoryBackingHotSwap { .. }),
            Err(EngineError::MemoryBackingChangedUnderStart { .. }),
        ) => panic!("contradictory pair: the attach both refused and changed the row"),
        (other_attached, other_started) => {
            panic!("unexpected race outcome (attach {other_attached:?}, start {other_started:?})")
        }
    }
    stop_best_effort(&engine_a, "race");
}

// ---------------------------------------------------------------------------
// The busy bound: two concurrent writers → bounded wait → typed busy error
// ---------------------------------------------------------------------------

#[test]
fn concurrent_engines_degrade_to_the_typed_busy_error_under_a_held_write_txn() {
    // The concurrent-writers row of the matrix, end to end through a second
    // ENGINE (not a raw store): a held write txn costs the attach one full
    // busy_timeout wait, then the TYPED busy error surfaces — never a raw
    // SQLITE_BUSY string (AI-18). Costs ~5s: that IS the bounded-wait
    // contract under test.
    let state = TempDir::new().unwrap();
    let engine_b = open(&state);
    let facade_b = engine_b.blocking();
    facade_b.register("busy", "mock").unwrap();

    let holder = raw_conn(&state);
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started_at = Instant::now();
    let err = facade_b
        .attach_memory("busy", MemoryBackingKind::Native)
        .unwrap_err();
    let waited = started_at.elapsed();

    match &err {
        RegistryError::Store(StoreError::Busy { waited_ms }) => {
            assert_eq!(*waited_ms, 5_000, "the stop-ladder bounded window");
        }
        other => panic!("expected the typed Busy error, got {other:?}"),
    }
    assert!(
        waited >= Duration::from_millis(5_000),
        "the bounded wait must elapse before the typed error: {waited:?}"
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("bounded") && !rendered.contains("SQLITE_BUSY"),
        "the operator surface names the bound, never a raw SQLITE_BUSY: {rendered}"
    );
    drop(holder);
}
