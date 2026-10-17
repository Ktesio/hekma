//! Integration tests for story 15-6: memory attach/detach × start atomicity —
//! the durable-store transactions closing the CROSS-PROCESS windows, driven
//! through the engine's PUBLIC API + blocking facade (spine AD-2).
//!
//! The real CLI shape is one `Engine` per command sharing a state dir, so the
//! racers here are TWO independent `Engine`s opened over ONE hermetic root
//! (the embed-clean collision test's shape, minus the different roots). The
//! only state two CLI processes share is the SQLite file — so the racer side
//! of the deterministic windows is a raw connection on that same file,
//! committing exactly the row/state mutation a racing `hekma agent memory
//! attach` / `detach` / `start` would commit.
//!
//! What is pinned here (the spec's I/O matrix, row by row):
//!
//! * **Window 1 (attach in start's read→transition window)** — the `→
//!   starting` transition transaction sees the attach row, disagrees with the
//!   start's pre-check read, refuses TYPED + retryable, and rolls back: no
//!   `starting` row, no launch, the other writer's commit stands.
//! * **Detach mirror** — same dichotomy for a mid-start detach.
//! * **Window 2 (attach after the transition txn)** — the attach guard
//!   refuses side-effect-free on the non-terminal instance (no hot-swap).
//! * **Window 3 (transition inside the attach guard→write gap)** — the
//!   attach's guard re-evaluates UNDER the write lock: no backing row lands
//!   on a non-terminal instance, row byte-unchanged.
//! * **Busy bound** — two concurrent writers degrade to the `busy_timeout`
//!   bounded wait and then the TYPED busy error, never a raw SQLITE_BUSY.
//! * **The unsynchronized two-Engine race** — attach and start fired
//!   together; the ONLY accepted outcomes are the matrix's dichotomy: start
//!   launches WITH the attached dir, or start rejects typed — never a
//!   launched agent contradicting the row.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::ports::StoreError;
use hekma_engine::{
    paths::EnginePaths, AdapterRef, Engine, EngineError, LifecycleState, MemoryBackingKind,
    RegistryError,
};
use rusqlite::Connection;
use tempfile::TempDir;

/// The bounded poll for a COMMITTED in-window artifact (the effective-config
/// snapshot file start writes BEFORE the transition transaction — its
/// existence proves the start has passed its pre-check reads and is parked at
/// the store transaction). Same committed-artifact polling discipline as the
/// sibling suites (no fixed sleeps to await state).
const WINDOW_POLL_BUDGET: Duration = Duration::from_secs(10);

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
        EnginePaths::new(Some(state.path().to_path_buf()))
            .expect("state paths")
            .state_db(),
    )
    .expect("open the raw racer connection")
}

/// The managed memory dir path (engine path authority).
fn memory_dir(state: &TempDir, name: &str) -> PathBuf {
    EnginePaths::new(Some(state.path().to_path_buf()))
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

    // TWO engines over ONE root: the real one-Engine-per-command shape.
    let engine_a = open(&state);
    let facade_a = engine_a.blocking();
    let _engine_b = open(&state);
    let registered = facade_a
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest_path))
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

    // THE PIN: typed + retryable, naming both sides — never a launch.
    match start_result {
        Err(EngineError::MemoryBackingChangedUnderStart {
            ref precheck,
            ref snapshot,
            ..
        }) => {
            assert_eq!(precheck, "none", "the pre-check read nothing");
            assert_eq!(
                snapshot, "kind 'filesystem' attached at 2026-10-17T00:00:00Z",
                "the txn snapshot names the racing attach"
            );
        }
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
// The detach mirror (same window, opposite mutation)
// ---------------------------------------------------------------------------

#[test]
fn a_detach_landing_inside_starts_window_rejects_the_start_typed_and_rolls_back() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "svc");

    let engine_a = open(&state);
    let facade_a = engine_a.blocking();
    let registered = facade_a
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest_path))
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
        // The attachment existed before the start; the racing detach clears
        // the row INSIDE the window.
        // The racing detach: the row CLEARS inside the window.
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

    match start_result {
        Err(EngineError::MemoryBackingChangedUnderStart {
            ref precheck,
            ref snapshot,
            ..
        }) => {
            assert!(
                precheck.starts_with("kind 'filesystem'"),
                "the pre-check saw the attachment: {precheck}"
            );
            assert_eq!(snapshot, "none", "the txn snapshot saw the clear");
        }
        Ok(_) => panic!("start launched while the backing was detached under it"),
        Err(other) => panic!("expected the typed mismatch error, got {other:?}"),
    }

    let conn = raw_conn(&state);
    assert_eq!(persisted_state(&conn, "svc"), "registered");
    assert_eq!(persisted_backing_kind(&conn, "svc"), None);
    assert!(
        !memory_dir(&state, "svc").join("nonexistent-probe").exists()
            && engine_a
                .blocking()
                .transition_events("svc")
                .unwrap()
                .is_empty(),
        "the rolled-back start records no transition"
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
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest_path))
        .unwrap();

    // The start's transition txn commits FIRST (no racer): the instance runs.
    let started = facade_a.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The racing attach lands AFTER the transition: the guard refuses with
    // the non-terminal refusal — no hot-swap onto a live instance, row
    // unchanged (the forbidden row would be the hot-swap).
    let err = facade_a
        .attach_memory("svc", hekma_engine::MemoryBackingKind::Filesystem)
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
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest_path))
        .unwrap();

    // The racer holds the write lock; the attach's PRE-CHECK guard reads the
    // terminal `registered` state through (WAL readers never block) and then
    // PARKS at its transaction.
    let racer = raw_conn(&state);
    racer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let attach_result = std::thread::scope(|s| {
        let handle = s.spawn(move || {
            facade_b.attach_memory("svc", hekma_engine::MemoryBackingKind::Filesystem)
        });
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
// The unsynchronized two-Engine race: the dichotomy invariant
// ---------------------------------------------------------------------------

#[test]
fn two_engines_racing_attach_against_start_only_ever_launch_with_the_row() {
    // No synchronization: whichever writer wins the write lock, the ONLY
    // accepted outcomes are the matrix's dichotomy — start launches WITH the
    // attached dir (attach committed before start's authoritative read), or
    // start rejects typed (attach committed inside the window), or the
    // attach refuses (it lost the race to the transition). A launched agent
    // contradicting the row is the forbidden outcome this test exists to
    // catch.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_path = write_lingering_manifest(manifest.path(), "race");

    let engine_a = open(&state);
    let engine_b = open(&state);
    engine_a
        .blocking()
        .register_with_adapter("race", &AdapterRef::Manifest(manifest_path))
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

    // The attach side accepts: success (it won the write lock before the
    // transition) or the typed hot-swap refusal (the transition won). Nothing
    // else.
    match &attached {
        Ok(_) | Err(RegistryError::MemoryBackingHotSwap { .. }) => {}
        other => panic!("unexpected attach outcome: {other:?}"),
    }

    let conn = raw_conn(&state);
    let row_kind = persisted_backing_kind(&conn, "race");
    match started {
        Ok(instance) => {
            // THE INVARIANT: a launched agent agrees with the row — attached.
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
        Err(EngineError::MemoryBackingChangedUnderStart { .. }) => {
            // The typed mid-start rejection: nothing launched, state rolled
            // back, and the racing attach's truth (whatever it was) stands.
            assert_eq!(persisted_state(&conn, "race"), "registered");
        }
        Err(other) => panic!("unexpected start outcome: {other:?}"),
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
        .attach_memory("busy", hekma_engine::MemoryBackingKind::Native)
        .unwrap_err();
    let waited = started_at.elapsed();

    match &err {
        RegistryError::Store(StoreError::Busy { waited_ms }) => {
            assert_eq!(*waited_ms, 5_000, "the stop-ladder bounded wait");
        }
        other => panic!("expected the typed Busy error, got {other:?}"),
    }
    assert!(
        waited >= Duration::from_millis(5_000),
        "the bounded wait must elapse before the typed error: {waited:?}"
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("bounded wait") && !rendered.contains("SQLITE_BUSY"),
        "the operator surface names the bound, never a raw SQLITE_BUSY: {rendered}"
    );
    drop(holder);
}
