use super::*;
use crate::domain::{AgentInstance, InstanceName, LifecycleState};
use crate::ports::StateStore;
use std::sync::atomic::AtomicBool;

fn name(raw: &str) -> InstanceName {
    InstanceName::new(raw).unwrap()
}

fn instance(raw: &str) -> AgentInstance {
    AgentInstance {
        name: name(raw),
        kind: "mock".to_string(),
        state: LifecycleState::Registered,
        agent_home: format!("/x/agents/{raw}"),
        created_at: "2026-10-31T00:00:00Z".to_string(),
        updated_at: "2026-10-31T00:00:00Z".to_string(),
    }
}

/// A writer over a fresh file DB plus a read-only connection on it.
fn writer() -> (tempfile::TempDir, StoreWriter, SqliteStore) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    let store = SqliteStore::open(&path).unwrap();
    let writer = StoreWriter::spawn(store).unwrap();
    let reader = SqliteStore::open_read_only(&path).unwrap();
    (dir, writer, reader)
}

fn create(writer: &StoreWriter, raw: &'static str) -> Result<(), StoreError> {
    writer.call(move |ctx| ctx.store().create_instance(&instance(raw)))
}

/// Hold the writer busy inside a job for `hold`, from another thread.
fn occupy(writer: &Arc<StoreWriter>, hold: Duration) -> std::thread::JoinHandle<()> {
    let writer = Arc::clone(writer);
    let (started_tx, started_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        writer
            .call(move |_ctx| {
                started_tx.send(()).unwrap();
                std::thread::sleep(hold);
                Ok(())
            })
            .unwrap();
    });
    started_rx.recv().unwrap();
    handle
}

#[test]
fn an_acked_write_is_visible_to_the_reader_once_it_returns() {
    let (_dir, writer, reader) = writer();
    create(&writer, "alpha").unwrap();
    assert!(reader.get_instance(&name("alpha")).unwrap().is_some());
    assert!(writer.stats().batches.load(Ordering::Relaxed) >= 1);
}

#[test]
fn the_reader_connection_refuses_every_write() {
    let (_dir, _writer, reader) = writer();
    let err = reader.create_instance(&instance("rogue")).unwrap_err();
    let text = err.to_string().to_lowercase();
    assert!(
        text.contains("readonly") || text.contains("read-only") || text.contains("read only"),
        "SQLite itself refuses a write that bypasses the writer: {err}"
    );
}

#[test]
fn a_failed_job_rolls_back_alone_and_its_batch_neighbours_commit() {
    let (_dir, writer, reader) = writer();
    let writer = Arc::new(writer);
    create(&writer, "dup").unwrap();
    // Queue two jobs while the writer is busy, so they share the next batch.
    let busy = occupy(&writer, Duration::from_millis(200));
    let failing = {
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || {
            writer.call(|ctx| {
                // A write that lands, then a write that refuses: the whole
                // job must roll back.
                ctx.store().create_instance(&instance("ghost"))?;
                ctx.store().create_instance(&instance("dup"))
            })
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let ok = {
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || create(&writer, "beta"))
    };
    busy.join().unwrap();
    assert!(matches!(
        failing.join().unwrap(),
        Err(StoreError::DuplicateName { .. })
    ));
    ok.join().unwrap().unwrap();
    assert!(
        reader.get_instance(&name("ghost")).unwrap().is_none(),
        "the failed job's first write was rolled back"
    );
    assert!(reader.get_instance(&name("beta")).unwrap().is_some());
}

#[test]
fn a_job_still_queued_after_the_ack_bound_is_busy_and_never_applied() {
    let (_dir, writer, reader) = writer();
    let writer = Arc::new(writer);
    let busy = occupy(&writer, ACK_BOUND + Duration::from_millis(500));
    let started = Instant::now();
    let err = create(&writer, "late").unwrap_err();
    let waited = started.elapsed();
    assert!(
        matches!(err, StoreError::Busy { waited_ms: 1000 }),
        "the typed retryable error: {err:?}"
    );
    assert!(
        waited >= ACK_BOUND && waited < ACK_BOUND + Duration::from_millis(400),
        "the caller waited the bound, not the writer: {waited:?}"
    );
    busy.join().unwrap();
    // Let the writer reach (and skip) the abandoned job.
    create(&writer, "after").unwrap();
    assert!(
        reader.get_instance(&name("late")).unwrap().is_none(),
        "Busy means the write did not land"
    );
}

#[test]
fn another_process_holding_the_write_lock_makes_writes_busy_not_applied() {
    let (dir, writer, reader) = writer();
    let other = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let err = create(&writer, "contended").unwrap_err();
    assert!(matches!(err, StoreError::Busy { .. }), "{err:?}");
    other.execute_batch("COMMIT").unwrap();
    // The writer recovers once the lock is released (its BEGIN may still be
    // inside the busy wait for the abandoned job).
    let mut landed = false;
    for _ in 0..30 {
        if create(&writer, "released").is_ok() {
            landed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(landed, "the writer writes again after the lock is released");
    assert!(reader.get_instance(&name("contended")).unwrap().is_none());
}

#[test]
fn post_commit_hooks_run_after_commit_and_never_for_a_rolled_back_job() {
    let (_dir, writer, _reader) = writer();
    let committed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&committed);
    writer
        .call(move |ctx| {
            ctx.after_commit(move || flag.store(true, Ordering::SeqCst));
            Ok(())
        })
        .unwrap();
    assert!(
        committed.load(Ordering::SeqCst),
        "the hook ran before the ack"
    );

    let rolled_back = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&rolled_back);
    let result: Result<(), StoreError> = writer.call(move |ctx| {
        ctx.after_commit(move || flag.store(true, Ordering::SeqCst));
        Err(StoreError::Backend("refused".to_string()))
    });
    assert!(result.is_err());
    assert!(
        !rolled_back.load(Ordering::SeqCst),
        "no hook for a failed job"
    );
}

#[test]
fn a_panicking_job_is_a_typed_error_and_the_writer_keeps_going() {
    let (_dir, writer, reader) = writer();
    let result: Result<(), StoreError> = writer.call(|ctx| {
        ctx.store().create_instance(&instance("halfway"))?;
        panic!("a bug inside a job");
    });
    assert!(
        matches!(&result, Err(StoreError::Backend(detail)) if detail.contains("panicked")),
        "{result:?}"
    );
    create(&writer, "next").unwrap();
    assert!(reader.get_instance(&name("halfway")).unwrap().is_none());
    assert!(reader.get_instance(&name("next")).unwrap().is_some());
}

#[test]
fn a_batch_sqlite_rolled_back_on_its_own_answers_every_job_not_applied() {
    // A full disk or an I/O error makes SQLite roll the WHOLE transaction
    // back by itself. Simulated by a job ending the transaction itself: no
    // job of that batch may report success, and the next job in the batch
    // must not run outside the transaction (where its SAVEPOINT would
    // autocommit on its own).
    let (_dir, writer, reader) = writer();
    let writer = Arc::new(writer);
    let busy = occupy(&writer, Duration::from_millis(200));
    let breaking = {
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || {
            writer.call(|ctx| {
                ctx.store().create_instance(&instance("first"))?;
                ctx.store().conn().execute_batch("ROLLBACK").unwrap();
                Ok(())
            })
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let after = {
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || create(&writer, "second"))
    };
    busy.join().unwrap();
    assert!(breaking.join().unwrap().is_err());
    let err = after.join().unwrap().unwrap_err();
    assert!(err.to_string().contains("rolled back by SQLite"), "{err}");
    assert!(reader.get_instance(&name("first")).unwrap().is_none());
    assert!(
        reader.get_instance(&name("second")).unwrap().is_none(),
        "never applied outside the batch transaction"
    );
    create(&writer, "third").unwrap();
    assert!(reader.get_instance(&name("third")).unwrap().is_some());
}

#[test]
fn a_panicking_post_commit_hook_does_not_stop_the_writer() {
    let (_dir, writer, reader) = writer();
    writer
        .call(|ctx| {
            ctx.store().create_instance(&instance("hooked"))?;
            ctx.after_commit(|| panic!("a bug in a hook"));
            Ok(())
        })
        .unwrap();
    create(&writer, "later").unwrap();
    assert!(reader.get_instance(&name("hooked")).unwrap().is_some());
    assert!(reader.get_instance(&name("later")).unwrap().is_some());
}

#[test]
fn an_acked_write_flushes_a_coalescing_unacked_batch_at_once() {
    let (_dir, writer, reader) = writer();
    create(&writer, "solo").unwrap();
    for _ in 0..10 {
        writer
            .submit(|ctx| {
                ctx.store()
                    .set_state(&name("solo"), LifecycleState::Stopped)
            })
            .unwrap();
    }
    let started = Instant::now();
    create(&writer, "flush").unwrap();
    assert!(
        started.elapsed() < BATCH_WINDOW,
        "the acked write did not wait out the window"
    );
    assert_eq!(writer.stats().unacked_failures.load(Ordering::Relaxed), 0);
    assert_eq!(
        reader.get_instance(&name("solo")).unwrap().unwrap().state,
        LifecycleState::Stopped
    );
}

#[test]
fn a_failed_unacked_job_is_counted() {
    let (_dir, writer, _reader) = writer();
    writer
        .submit(|_ctx| Err(StoreError::Backend("refused".to_string())))
        .unwrap();
    create(&writer, "after").unwrap();
    assert_eq!(writer.stats().unacked_failures.load(Ordering::Relaxed), 1);
}

#[test]
fn a_dropped_writer_commits_what_was_queued() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    let writer = StoreWriter::spawn(SqliteStore::open(&path).unwrap()).unwrap();
    writer
        .submit(|ctx| ctx.store().create_instance(&instance("queued")))
        .unwrap();
    drop(writer);
    let reader = SqliteStore::open_read_only(&path).unwrap();
    assert!(reader.get_instance(&name("queued")).unwrap().is_some());
}

// ---- The writer bench (story 21-1's NFR-4 pass criterion) ----
//
// Seed shape: M instances × N usage events spread round-robin over one Run
// each, a token ceiling armed per instance so every event runs the full
// pipeline job (insert, rollups, outbox, evaluation, latch read). Gates: an
// unacked batch flushes within the 250 ms window of its first job, a single
// acked write is ≤ 50 ms p99, and usage-commit-to-decision ≤ 1 s. The
// default seed runs in the normal suite; the large one is `#[ignore]`d and
// sized by `HEKMA_WRITER_BENCH_INSTANCES` / `HEKMA_WRITER_BENCH_EVENTS`
// (the Run-scale numbers are measured from stories 21.5 and 24.4 on).

mod bench {
    use super::*;
    use crate::domain::pipeline::{commit_usage, EvalInput, UsageJob};
    use crate::domain::{BreachAction, CostCap, RunId, TokenBudget, UsageEvent};

    struct Report {
        acked_p99: Duration,
        breach_latency: Duration,
        events: usize,
        seeded_in: Duration,
    }

    fn job(instance: &str, run: &str, sequence: u64, ceiling: u64) -> UsageJob {
        UsageJob {
            event: UsageEvent {
                instance: instance.to_string(),
                run_id: RunId::from_wire(run),
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: Some(0),
                metering_source: "self-reported".to_string(),
                sequence,
                occurred_at: "2026-10-31T00:00:00Z".to_string(),
            },
            generation: 1,
            eval: EvalInput {
                name: name(instance),
                run_id: RunId::from_wire(run),
                metering_source: "self-reported".to_string(),
                token_budget: TokenBudget {
                    per_run: Some(ceiling),
                    cumulative: None,
                },
                token_action: BreachAction::Pause,
                rate: None,
                cost_cap: CostCap::default(),
                cost_action: BreachAction::Pause,
                supervised: true,
                fence: None,
                totals_fault: false,
            },
        }
    }

    fn run(instances: usize, events: usize) -> Report {
        let (_dir, writer, _reader) = writer();
        let names: Vec<String> = (0..instances).map(|i| format!("bench-{i}")).collect();
        for raw in &names {
            let row = instance(raw);
            writer
                .call(move |ctx| ctx.store().create_instance(&row))
                .unwrap();
        }
        // The seed goes unacked (the per-call-record shape), batched ≤ 512.
        let started = Instant::now();
        for index in 0..events {
            let raw = &names[index % instances];
            let job = job(
                raw,
                &format!("run-{raw}"),
                (index / instances) as u64,
                u64::MAX,
            );
            writer
                .submit(move |ctx| commit_usage(ctx, job).map(|_| ()))
                .unwrap();
        }
        // Wait for the seed to commit (an acked write queued behind 10k jobs
        // would hit its own 1 s bound, so poll the writer's counter).
        let target = (instances + events) as u64;
        while writer.stats().jobs.load(Ordering::Relaxed) < target {
            std::thread::sleep(Duration::from_millis(5));
        }
        let seeded_in = started.elapsed();
        assert_eq!(writer.stats().unacked_failures.load(Ordering::Relaxed), 0);
        // Steady-state single acked writes on the seeded ledger.
        let mut acked = Vec::new();
        for index in 0..100u64 {
            let raw = &names[(index as usize) % instances];
            let job = job(raw, &format!("run-{raw}"), 1_000_000_000 + index, u64::MAX);
            let one = Instant::now();
            writer.call(move |ctx| commit_usage(ctx, job)).unwrap();
            acked.push(one.elapsed());
        }
        acked.sort();
        // Usage commit to decision: one event crossing a ceiling commits its
        // breach in the same acked job.
        let crossing = job(&names[0], "run-breach", 0, 1);
        let one = Instant::now();
        let commit = writer.call(move |ctx| commit_usage(ctx, crossing)).unwrap();
        let breach_latency = one.elapsed();
        assert_eq!(commit.evaluation.breaches.len(), 1, "the crossing fired");
        Report {
            acked_p99: acked[acked.len() * 99 / 100 - 1],
            breach_latency,
            events,
            seeded_in,
        }
    }

    fn gate(report: &Report) {
        eprintln!(
            "store-writer bench: {} pipeline events seeded in {:?} ({:.0}/s); acked p99 {:?}; \
             usage-commit-to-decision {:?}",
            report.events,
            report.seeded_in,
            report.events as f64 / report.seeded_in.as_secs_f64(),
            report.acked_p99,
            report.breach_latency,
        );
        assert!(
            report.acked_p99 <= Duration::from_millis(50),
            "single acked write p99 {:?} > 50 ms",
            report.acked_p99
        );
        assert!(
            report.breach_latency <= Duration::from_secs(1),
            "usage-commit-to-decision {:?} > 1 s",
            report.breach_latency
        );
    }

    #[test]
    fn writer_bench_default_seed() {
        gate(&run(25, 10_000));
    }

    #[test]
    fn a_lone_unacked_batch_flushes_within_the_window() {
        let (_dir, writer, _reader) = writer();
        create(&writer, "window").unwrap();
        let jobs_before = writer.stats().jobs.load(Ordering::Relaxed);
        writer
            .submit(|ctx| {
                ctx.store()
                    .set_state(&name("window"), LifecycleState::Stopped)
            })
            .unwrap();
        let started = Instant::now();
        while writer.stats().jobs.load(Ordering::Relaxed) == jobs_before {
            assert!(started.elapsed() < Duration::from_secs(2), "never flushed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let flush = Duration::from_micros(
            writer
                .stats()
                .max_unacked_flush_micros
                .load(Ordering::Relaxed),
        );
        assert!(
            flush >= BATCH_WINDOW && flush <= BATCH_WINDOW + Duration::from_millis(150),
            "a lone unacked job waits out the 250 ms window, then flushes: {flush:?}"
        );
    }

    #[test]
    #[ignore = "the large writer bench — run with --ignored, sized by env"]
    fn writer_bench_large_seed() {
        let env = |key: &str, default: usize| {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        gate(&run(
            env("HEKMA_WRITER_BENCH_INSTANCES", 1_000),
            env("HEKMA_WRITER_BENCH_EVENTS", 1_000_000),
        ));
    }
}

#[test]
fn a_failed_commit_answers_every_job_in_the_batch_not_applied() {
    // A deferred foreign-key violation surfaces only at COMMIT: the whole
    // batch rolls back and every job hears so, typed.
    let (_dir, writer, reader) = writer();
    let result: Result<(), StoreError> = writer.call(|ctx| {
        ctx.store().create_instance(&instance("doomed"))?;
        ctx.store()
            .conn()
            .execute_batch(
                "PRAGMA defer_foreign_keys = ON; \
                 INSERT INTO usage_rollup_instance (instance_id) VALUES (9999);",
            )
            .unwrap();
        Ok(())
    });
    assert!(
        matches!(&result, Err(StoreError::Backend(detail)) if detail.contains("did not commit")),
        "{result:?}"
    );
    assert!(reader.get_instance(&name("doomed")).unwrap().is_none());
    create(&writer, "next").unwrap();
}
