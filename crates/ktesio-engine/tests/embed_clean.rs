//! Story 7-3 (FR-34 / AD-13): the embed-clean verification suite — the engine
//! embeds into a Host's process cleanly: headless, prompt-free, no global
//! process state that could collide with the Host's runtime, and the full API
//! reachable behind the blocking facade.
//!
//! ## The three verification instruments (and what they found)
//!
//! 1. **The two-engine collision test** — TWO independent engines in ONE
//!    process, different hermetic roots, each driving the FULL UJ-3 flow
//!    (register → configure → cap → start → breach → pause → stop)
//!    CONCURRENTLY (one thread per engine, barrier-synchronized start), each
//!    with its own story-7-2 subscriber. Both flows are asserted with the
//!    SHARED `ktesio_conformance::uj3` expectations (same expectations,
//!    independent roots), and isolation is asserted on four axes: each
//!    subscriber's received streams equal ITS OWN engine's committed truth
//!    exactly (transitions == the committed `instance.log`, breaches == the
//!    committed `breaches.log`, usage == the committed `usage_events` ledger
//!    rows field-for-field through the SHARED `uj3` projection — so any
//!    cross-engine leak would add or misorder an event against the local
//!    committed records), each root's DB holds exactly ONE instance (its
//!    own), and BOTH the usage and the breach event streams are disjoint
//!    across engines (Run ids included — the breach events carry `run_id`
//!    too). HONESTY about the concurrency: the barrier-synchronized start is
//!    ENFORCED (both engines are demonstrably open, subscribed, and running
//!    their flows in one process at the same time), but deeper step-by-step
//!    interleaving of the two flows is scheduler luck and is deliberately
//!    NOT claimed — the isolation guarantees do not depend on it.
//! 2. **The no-TTY/no-prompt/no-global-handler audit** — a durable source
//!    audit over the engine's production sources asserting the engine crate
//!    never reads stdin, never prints an interactive prompt, never DETECTS a
//!    terminal (branching on terminal-ness violates no-TTY without a prompt),
//!    never mutates the process environment (`set_var`/`remove_var` —
//!    reading `KTESIO_STATE_DIR` is fine), never installs a process-global
//!    handler (signal hooks, panic hooks, console control/exception hooks,
//!    `tokio::signal`), never builds a lazy/global runtime or static cell
//!    outside the per-engine runtime `Engine::open` owns, and holds exactly
//!    ONE named allowlist entry the audit honestly earned — ONE global
//!    static:
//!    * `static RUN_NONCE: AtomicU64` (`domain/usage.rs`) — the RunId
//!      uniqueness tie-breaker: a monotonic counter consulted only to keep
//!      two same-nanosecond Run ids DISTINCT, never read for behavior. It
//!      guarantees per-PROCESS uniqueness — exactly what a multi-engine
//!      single-process host needs. Cross-process uniqueness is NOT claimed
//!      (separate roots keep separate ledgers; conflation would only matter
//!      for merged ledgers, which Ktesio does not do). The collision test's
//!      disjoint-run-id assertion proves this per-process property.
//!
//!    The audit's TWO historical print-site allowlist entries — the best-effort
//!    stderr diagnostics in `domain/supervisor.rs` (the DC-10 memory-delivery
//!    notice and the enforcement breadcrumb, both citing spine AD-12) — were
//!    CLOSED by story 10-2, not allowlisted forever: the pinned diagnostics now
//!    route through the host-provided diagnostic sink (`Supervisor::
//!    emit_diagnostic`; stderr survives only as the no-sink DEFAULT, which the
//!    `diagnostic_sink.rs` suite proves byte-identical). The print-site
//!    allowlist is therefore EMPTY — any new raw print in production sources
//!    fails the audit outright — and the sink plumbing itself carries positive
//!    pins with the same count==1 discipline the old entries had: the
//!    `emit_diagnostic` choke point, BOTH diagnostic routes into it, and the
//!    stderr default arm must each match EXACTLY ONE site, so a diagnostic
//!    cannot silently disappear, be reworded, or grow a second direct stderr
//!    writer.
//!
//!    The scanner itself is hardened against bypass classes: string-literal
//!    contents are blanked before token matching (a log message containing
//!    "stdin" cannot match), comments are stripped (line AND block), test
//!    modules are skipped as REGIONS (production code declared after a test
//!    module cannot escape the scan — an earlier truncate-at-first-marker
//!    heuristic had exactly that blind spot, which is how the two stderr
//!    diagnostics were originally missed), `#[cfg(...)]` gates are
//!    recognized as test gates only when they actually select test builds
//!    (`not(test)` keeps production code scanned), and an unreadable source
//!    file or directory PANICS with its path instead of silently meaning
//!    "unscanned". The story-10-2 stdio accounting is widened against the
//!    imported-path bypass: the single-stdio-reach count pin matches
//!    fully-qualified calls, BARE imported-path calls
//!    (`use std::io::stdout;` + `stdout()`), brace-group stdio imports, and
//!    hand-written `_print` internals — so shortening a stdio path to an
//!    imported name cannot sneak a second writer past either the zero-print
//!    scan or the count pin.
//! 3. **The blocking-coverage inventory audit** — every `pub async fn` in
//!    the WHOLE production crate (not just `engine.rs` — an
//!    `impl Engine { pub async fn … }` in another module cannot escape the
//!    count) is matched against the [`Blocking`] facade's inventory PARSED
//!    FROM `engine.rs`, by NAME AND SIGNATURE: each counterpart's
//!    whitespace-normalized parameter list must equal the async method's, so
//!    arity/type drift cannot silently diverge the surfaces (FR-34's
//!    "covers the full async API"). Each counterpart must actually bridge via
//!    `block_on`, and the facade's only intentional extra is the bridged
//!    `subscribe` (the sync [`EventSubscription`] over the sync
//!    [`Engine::subscribe`]). The kt extension asserts `kt`'s own sources
//!    never touch an engine async API or a runtime (no `.await`, no
//!    `async {`, no `tokio::`, no `futures::`, no `block_on`, no
//!    `now_or_never`) — matched against comment- and string-cleaned code
//!    lines — and positively drives `.blocking()` on a code line; the "no
//!    tokio dependency" claim is pinned structurally by asserting
//!    `crates/kt/Cargo.toml` declares no tokio edge. The full build-level
//!    boundary proof remains story 7-4's.
//!
//! ## Determinism posture (the house style, shared)
//!
//! The collision test follows the 7-1/7-2 posture unchanged: the fixture
//! emits a KNOWN batch with FIXED token sentinels, and every wait polls
//! COMMITTED state through the shared bounded poller (`uj3::wait_for_state`
//! — committed-state polling with a bounded retry loop, never a wall-clock
//! guess against a side effect). The subscriber drain is exact (every
//! publish completes under the supervisor lock before its facade call
//! returns, so after the committed-state wait plus ONE supervisor-lock-taking
//! `fleet()` read — the barrier — the shared `try_recv`-until-empty drain is
//! exact, never racy). No `OsId` gate; runs unmodified on all three OSes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::thread;

use ktesio_conformance::uj3;
use ktesio_engine::{broadcast, AdapterRef, ConfigLayer, Engine, EngineEvent, LifecycleState};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Instrument 1: the two-engine concurrent collision test
// ---------------------------------------------------------------------------

/// What one concurrent flow thread hands back: its root plus the EXACT
/// subscriber stream it observed (drained after the flow settled).
struct FlowResult {
    root: PathBuf,
    events: Vec<EngineEvent>,
    /// `Some(n)` if the receiver observed a `Lagged(n)` (it must not — the
    /// flow emits a handful of events, far below
    /// [`EVENT_BUS_CAPACITY`](ktesio_engine::EVENT_BUS_CAPACITY)).
    lagged: Option<u64>,
}

#[test]
fn two_engines_in_one_process_drive_the_full_uj3_flow_concurrently_without_collision() {
    // The fake_agent helper must exist before the threads spawn: its lookup
    // can fall back to an on-demand cargo build, and one builder thread (this
    // main thread) keeps that arm off the concurrent paths entirely.
    let _warm = ktesio_conformance::fake_agent_bin();

    let state_a = TempDir::new().expect("engine-A state root");
    let state_b = TempDir::new().expect("engine-B state root");
    let manifest_a = TempDir::new().expect("engine-A manifest dir");
    let manifest_b = TempDir::new().expect("engine-B manifest dir");
    let manifest_dir_a = uj3::write_flow_manifest(manifest_a.path());
    let manifest_dir_b = uj3::write_flow_manifest(manifest_b.path());

    // ALL pre-barrier setup happens on THIS main thread: both engines are
    // open and subscribed before either flow thread exists. A setup failure
    // fails the test directly — no thread can be left blocked on the barrier
    // by a sibling's early panic (a hung CI is not a red test).
    let engine_a = Engine::open(Some(state_a.path().to_path_buf())).expect("open engine A");
    let mut sub_a = engine_a.subscribe();
    let engine_b = Engine::open(Some(state_b.path().to_path_buf())).expect("open engine B");
    let mut sub_b = engine_b.subscribe();
    let barrier = Barrier::new(2);

    // One thread per engine, synchronized start. `thread::scope` joins BOTH
    // threads before the scope returns — a panicking A can never leave B
    // detached against dropped roots.
    let (joined_a, joined_b) = thread::scope(|s| {
        let handle_a = s.spawn(|| {
            drive_full_uj3_flow(
                &engine_a,
                &mut sub_a,
                state_a.path(),
                &manifest_dir_a,
                &barrier,
            )
        });
        let handle_b = s.spawn(|| {
            drive_full_uj3_flow(
                &engine_b,
                &mut sub_b,
                state_b.path(),
                &manifest_dir_b,
                &barrier,
            )
        });
        (handle_a.join(), handle_b.join())
    });

    // BOTH flows completed correctly — every shared `uj3` expectation passed
    // INSIDE the threads (a thread panic surfaces here, after both joined).
    let result_a = joined_a.expect("engine-A flow thread panicked");
    let result_b = joined_b.expect("engine-B flow thread panicked");

    // ---- Isolation, per engine: each subscriber saw ONLY its own engine's
    // committed truth (exact stream equality is the leak detector — a leaked
    // foreign event would be an extra/misordered element against the local
    // committed log/ledger). ----
    assert_subscriber_isolation(&result_a);
    assert_subscriber_isolation(&result_b);

    // ---- Isolation, cross-engine: separate roots, one instance row per
    // root, and disjoint Run-id sets on BOTH event families (the same
    // instance NAME ran twice; each Run belongs to exactly one engine's
    // ledger — usage AND breach events alike carry `run_id`). ----
    let usage_runs_a = ledger_usage_run_ids(&result_a.root);
    let usage_runs_b = ledger_usage_run_ids(&result_b.root);
    assert!(
        !usage_runs_a.is_empty() && !usage_runs_b.is_empty(),
        "both flows committed usage rows under their own roots"
    );
    let usage_overlap: Vec<&String> = usage_runs_a.intersection(&usage_runs_b).collect();
    assert!(
        usage_overlap.is_empty(),
        "the two engines' usage Run-id sets must be disjoint (a shared Run id \
         would mean shared ledger identity): overlap = {usage_overlap:?}"
    );

    // The direct no-cross-engine-event-leakage checks: each subscriber's
    // received streams must be disjoint from the OTHER engine's committed
    // Run-id sets — for usage AND for breaches (both families carry run_id).
    let breach_runs_a = ledger_breach_run_ids(&result_a.root);
    let breach_runs_b = ledger_breach_run_ids(&result_b.root);
    assert!(
        !breach_runs_a.is_empty() && !breach_runs_b.is_empty(),
        "both flows committed breach records under their own roots"
    );
    let recv_usage_a = received_usage_run_ids(&result_a.events);
    let recv_usage_b = received_usage_run_ids(&result_b.events);
    let recv_breach_a = received_breach_run_ids(&result_a.events);
    let recv_breach_b = received_breach_run_ids(&result_b.events);
    assert!(
        recv_usage_a.is_disjoint(&usage_runs_b),
        "engine A's subscriber saw engine B's usage: {recv_usage_a:?} ∩ {usage_runs_b:?}"
    );
    assert!(
        recv_usage_b.is_disjoint(&usage_runs_a),
        "engine B's subscriber saw engine A's usage: {recv_usage_b:?} ∩ {usage_runs_a:?}"
    );
    assert!(
        recv_breach_a.is_disjoint(&breach_runs_b),
        "engine A's subscriber saw engine B's breaches: {recv_breach_a:?} ∩ {breach_runs_b:?}"
    );
    assert!(
        recv_breach_b.is_disjoint(&breach_runs_a),
        "engine B's subscriber saw engine A's breaches: {recv_breach_b:?} ∩ {breach_runs_a:?}"
    );
}

/// Drive the FULL UJ-3 flow (the shared 7-1 expectations, unchanged) on ONE
/// engine inside this thread, with a story-7-2 subscriber attached BEFORE any
/// traffic, and return the drained subscriber stream. The engine and the
/// receiver are opened/subscribed on the MAIN thread and borrowed here, so
/// the barrier is the flow's FIRST statement — nothing before it can panic
/// and hang the sibling thread.
fn drive_full_uj3_flow(
    engine: &Engine,
    sub: &mut broadcast::Receiver<EngineEvent>,
    root: &Path,
    manifest: &Path,
    barrier: &Barrier,
) -> FlowResult {
    let facade = engine.blocking();
    // The synchronized start: both engines are demonstrably alive and
    // subscribed in one process before either flow drives. (Deeper
    // step-by-step interleaving is scheduler luck and is NOT claimed — the
    // isolation guarantees do not depend on it.)
    barrier.wait();

    // ---- §4.1 register: a manifest adapter, through the facade. ----
    facade
        .register_with_adapter(
            uj3::FLOW_INSTANCE,
            &AdapterRef::Manifest(manifest.to_path_buf()),
        )
        .expect("register the flow's agent");

    // ---- §4.3 + §4.5 configure: the SHARED key/value pairs. ----
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(uj3::FLOW_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }

    // ---- §4.3 read back (the shared provenance assertion). ----
    let effective = facade
        .effective_config(uj3::FLOW_INSTANCE, ConfigLayer::empty())
        .expect("effective config read");
    uj3::assert_model_leaf(
        &effective
            .value_display(uj3::MODEL_KEY)
            .expect("the configured model key resolves"),
        effective
            .source_label(uj3::MODEL_KEY)
            .expect("the configured model key carries provenance"),
    );

    // ---- §4.1 pre-start Fleet read (the shared assertion). ----
    let entry = fleet_entry(&facade);
    uj3::assert_pre_start_entry(&entry);

    // ---- §4.2 start; §4.5 breach → pause on COMMITTED state (the shared
    // bounded committed-state poller, never a wall-clock guess). ----
    facade
        .start(uj3::FLOW_INSTANCE)
        .expect("start the flow's agent");
    uj3::wait_for_state(
        root,
        uj3::FLOW_INSTANCE,
        LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );

    // The breach chain, through the facade AND the committed log (shared).
    let breaches = facade
        .budget_breach_events(uj3::FLOW_INSTANCE)
        .expect("breach events read");
    uj3::assert_flow_breaches(&breaches);
    uj3::assert_flow_breaches(&uj3::read_breach_events(root, uj3::FLOW_INSTANCE));
    let transitions = facade
        .transition_events(uj3::FLOW_INSTANCE)
        .expect("transition events read");
    uj3::assert_paused_transition_budget_exceeded(&transitions);
    uj3::assert_paused_entry(&fleet_entry(&facade));

    // ---- §4.2 stop (the shared zero window) + the terminal readback. ----
    facade
        .stop(uj3::FLOW_INSTANCE, Some(uj3::STOP_WINDOW))
        .expect("stop the flow's agent");
    uj3::assert_stopped_entry(&fleet_entry(&facade));

    // The terminal `fleet()` read took the supervisor lock AFTER every
    // publish returned — the drain barrier. The SHARED `try_recv`-until-empty
    // drain is now exact, never racy (the 7-2 posture).
    let (events, lagged) = uj3::drain_receiver(sub);
    FlowResult {
        root: root.to_path_buf(),
        events,
        lagged,
    }
}

/// The isolation assertions for ONE engine's drained subscriber stream.
fn assert_subscriber_isolation(result: &FlowResult) {
    // No lag: the flow emits a handful of events, far below the capacity.
    assert_eq!(
        result.lagged, None,
        "the flow must never approach the bus capacity"
    );

    // Every received event is instance-scoped to THIS flow's instance.
    for event in &result.events {
        assert_eq!(
            event_instance(event),
            uj3::FLOW_INSTANCE,
            "a foreign-instance event arrived: {event:?}"
        );
    }

    // Received transitions == the committed transition log, EXACTLY (publish
    // order == commit order, one publish per committed append — story 7-2).
    let received: Vec<&ktesio_engine::TransitionEvent> = result
        .events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Transition(t) => Some(t),
            _ => None,
        })
        .collect();
    let committed = uj3::read_transition_events(&result.root, uj3::FLOW_INSTANCE);
    assert!(
        !committed.is_empty(),
        "the flow committed transitions under its own root"
    );
    assert_eq!(
        received,
        committed.iter().collect::<Vec<_>>(),
        "the received transition stream must equal the committed instance.log exactly \
         (any cross-engine leak would be an extra/misordered element)"
    );

    // Received breaches == the committed breach log, EXACTLY (two dimensions).
    let received_breaches: Vec<&ktesio_engine::BudgetBreachEvent> = result
        .events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::BudgetBreach(b) => Some(b),
            _ => None,
        })
        .collect();
    let committed_breaches = uj3::read_breach_events(&result.root, uj3::FLOW_INSTANCE);
    assert_eq!(
        received_breaches,
        committed_breaches.iter().collect::<Vec<_>>(),
        "the received breach stream must equal the committed breaches.log exactly"
    );

    // Received usage == the committed ledger rows, field-for-field, in commit
    // order (run_id included — the strongest per-engine identity check),
    // through the SHARED `uj3` projection both consuming suites use.
    let received_usage: Vec<uj3::CommittedUsage> = result
        .events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UsageUpdate(u) => Some(uj3::usage_from_payload(u)),
            _ => None,
        })
        .collect();
    let rows = uj3::committed_usage_rows(&result.root, uj3::FLOW_INSTANCE);
    assert!(
        !rows.is_empty(),
        "the flow committed usage rows under its own root"
    );
    assert_eq!(
        received_usage, rows,
        "the received usage stream must equal the committed ledger rows exactly"
    );

    // The instance row lives under THIS root only (each engine's DB holds
    // exactly one instance — its own).
    assert_eq!(
        instance_row_count(&result.root),
        1,
        "exactly one instance is registered under this engine's root"
    );
}

/// The instance name an [`EngineEvent`] payload carries (all three payload
/// families are instance-scoped).
fn event_instance(event: &EngineEvent) -> &str {
    match event {
        EngineEvent::Transition(t) => &t.instance,
        EngineEvent::BudgetBreach(b) => &b.instance,
        EngineEvent::UsageUpdate(u) => &u.event.instance,
    }
}

/// The Run ids in one engine's committed usage ledger (a set, for the
/// disjointness and no-cross-leak checks).
fn ledger_usage_run_ids(root: &Path) -> BTreeSet<String> {
    uj3::committed_usage_rows(root, uj3::FLOW_INSTANCE)
        .into_iter()
        .map(|row| row.run_id)
        .collect()
}

/// The Run ids in one engine's committed breach log (breach events carry
/// `run_id` too — the cross-engine disjointness check covers this family).
fn ledger_breach_run_ids(root: &Path) -> BTreeSet<String> {
    uj3::read_breach_events(root, uj3::FLOW_INSTANCE)
        .iter()
        .map(|b| b.run_id.clone())
        .collect()
}

/// The Run ids in a drained subscriber's usage events.
fn received_usage_run_ids(events: &[EngineEvent]) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UsageUpdate(u) => Some(u.event.run_id.as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// The Run ids in a drained subscriber's breach events.
fn received_breach_run_ids(events: &[EngineEvent]) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::BudgetBreach(b) => Some(b.run_id.clone()),
            _ => None,
        })
        .collect()
}

/// The number of registered instances in the engine state DB under `root`
/// (opened through the engine-published `STATE_DB_FILE` name, never a
/// hand-typed file name).
fn instance_row_count(root: &Path) -> u64 {
    let conn = rusqlite::Connection::open(root.join(ktesio_engine::paths::STATE_DB_FILE))
        .expect("open state db");
    conn.query_row("SELECT COUNT(*) FROM agent_instances", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n.max(0) as u64)
    .expect("count the instance rows")
}

/// The Fleet detail row for the flow's instance, through the facade read.
fn fleet_entry(facade: &ktesio_engine::Blocking<'_>) -> ktesio_engine::FleetEntry {
    facade
        .fleet()
        .expect("fleet read")
        .into_iter()
        .find(|e| e.name.as_str() == uj3::FLOW_INSTANCE)
        .expect("the flow's instance is in the Fleet")
}

// ---------------------------------------------------------------------------
// Instruments 2 + 3: the durable source audits (and their scanner)
// ---------------------------------------------------------------------------

/// Recursively visit every `.rs` file under `dir`, calling `f(rel_path,
/// contents)` with the `/`-normalized path relative to `dir` (the
/// `budget.rs` audit walker, extended with the relative path). An unreadable
/// FILE or DIRECTORY PANICS with its path — a read failure must never
/// silently mean "unscanned": a directory whose `read_dir` fails would
/// otherwise vacuously pass the audit as an unvisited subtree.
fn visit_rs(dir: &Path, f: &mut impl FnMut(&Path, &str)) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "could not read directory {} for the audit scan: {e}",
            dir.display()
        )
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("could not read {} for the audit scan: {e}", path.display())
            });
            f(&path, &text);
        }
    }
}

/// Blank the CONTENTS of double-quoted string literals (keeping the quotes)
/// so audited tokens inside message strings can never match, while code
/// tokens outside strings still do. Raw strings (`r#"…"#`) do not occur in
/// the scanned production sources today (the manifest builders live in test
/// modules, which are skipped as regions anyway).
fn blank_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            out.push('"');
            while let Some(inner) = chars.next() {
                match inner {
                    '\\' => {
                        chars.next();
                    }
                    '"' => {
                        out.push('"');
                        break;
                    }
                    _ => out.push(' '),
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Clean ONE physical line: blank string contents, cut `//` line comments
/// (string-aware, so `"http://…"` survives), and handle `/* … */` block
/// comments (state carried across lines via `in_block`). Returns the code
/// text of the line (possibly empty).
fn clean_line(line: &str, in_block: &mut bool) -> String {
    let mut rest = line;
    let mut out = String::new();
    loop {
        if *in_block {
            match rest.find("*/") {
                Some(end) => {
                    rest = &rest[end + 2..];
                    *in_block = false;
                }
                None => return out,
            }
        }
        // Find the first comment opener outside a string.
        let bytes = rest.as_bytes();
        let mut in_str = false;
        let mut cut: Option<(usize, bool)> = None; // (pos, line_comment)
        let mut k = 0;
        while k < bytes.len() {
            let c = bytes[k];
            if in_str {
                match c {
                    b'\\' => k += 1,
                    b'"' => in_str = false,
                    _ => {}
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'/' if k + 1 < bytes.len() && bytes[k + 1] == b'/' => {
                        cut = Some((k, true));
                        break;
                    }
                    b'/' if k + 1 < bytes.len() && bytes[k + 1] == b'*' => {
                        cut = Some((k, false));
                        break;
                    }
                    _ => {}
                }
            }
            k += 1;
        }
        match cut {
            None => {
                out.push_str(&blank_strings(rest));
                return out;
            }
            Some((pos, true)) => {
                out.push_str(&blank_strings(&rest[..pos]));
                return out;
            }
            Some((pos, false)) => {
                out.push_str(&blank_strings(&rest[..pos]));
                match rest[pos + 2..].find("*/") {
                    Some(end_rel) => {
                        rest = &rest[pos + 2 + end_rel + 2..];
                    }
                    None => {
                        *in_block = true;
                        return out;
                    }
                }
            }
        }
    }
}

/// True when the cleaned attribute line is a TEST gate: `#[cfg(test)]` or a
/// compound gate selecting test builds (`all(test, …)`), but NOT
/// `not(test)` — which selects PRODUCTION code under test compilation and
/// must keep being scanned.
fn cfg_test_gate(trimmed: &str) -> bool {
    let Some(expr) = trimmed.strip_prefix("#[cfg(") else {
        return false;
    };
    let Some(expr) = expr.strip_suffix(")]") else {
        return false;
    };
    let tokens: Vec<&str> = expr
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .collect();
    tokens
        .iter()
        .enumerate()
        .any(|(i, t)| *t == "test" && (i == 0 || tokens[i - 1] != "not"))
}

/// Clean a whole file into line-ALIGNED code text (same line count; skipped
/// lines become empty) so findings can report the ORIGINAL line at the same
/// index. Two passes:
///
/// 1. per line: string contents blanked, comments stripped (block-comment
///    state carried across lines);
/// 2. test modules SKIPPED AS REGIONS, not truncation: a `#[cfg(test)]`
///    (or compound `all(test, …)`) gate followed by `mod …` starts a skipped
///    brace-tracked region — production code declared AFTER a test module
///    cannot escape the scan (an earlier truncate-at-first-marker heuristic
///    had exactly that blind spot). Consecutive attribute lines after the
///    gate are part of the skipped declaration, and an ITEM-LEVEL
///    `#[cfg(test)]` attribute (a test-injection helper such as
///    `Supervisor::with_backoff` mid-file) is NOT a boundary: its production
///    code keeps being scanned — that distinction is load-bearing.
fn clean_source(text: &str) -> Vec<String> {
    let mut in_block = false;
    let mut cleaned: Vec<String> = text.lines().map(|l| clean_line(l, &mut in_block)).collect();

    let mut i = 0;
    while i < cleaned.len() {
        if cfg_test_gate(cleaned[i].trim()) {
            // Lookahead: consecutive attribute lines may follow the gate.
            let mut j = i + 1;
            while j < cleaned.len() {
                let t = cleaned[j].trim();
                if t.is_empty() || t.starts_with("#[") {
                    j += 1;
                    continue;
                }
                break;
            }
            if j < cleaned.len() && cleaned[j].trim_start().starts_with("mod ") {
                // Brace-track the module body (strings/comments are already
                // cleaned away, so braces here are code).
                let mut depth: i32 = 0;
                let mut started = false;
                let mut bodyless = false;
                let mut k = j;
                while k < cleaned.len() {
                    for ch in cleaned[k].chars() {
                        match ch {
                            '{' => {
                                depth += 1;
                                started = true;
                            }
                            '}' => depth -= 1,
                            _ => {}
                        }
                    }
                    if started && depth <= 0 {
                        break;
                    }
                    if !started && cleaned[k].trim_end().ends_with(';') {
                        bodyless = true;
                        break;
                    }
                    k += 1;
                }
                let last = if bodyless { j } else { k };
                let zero_end = (last.min(cleaned.len() - 1) + 1).min(cleaned.len());
                for line in cleaned.iter_mut().take(zero_end).skip(i) {
                    *line = String::new();
                }
                i = last + 1;
                continue;
            }
            // Item-level attribute — not a boundary; keep scanning.
        }
        i += 1;
    }
    cleaned
}

/// A finding: the `/`-normalized relative path, the 1-based line number, and
/// the ORIGINAL trimmed line (display); matching runs on the CLEANED text.
struct Finding {
    file: String,
    line: usize,
    text: String,
}

impl Finding {
    fn describe(&self) -> String {
        format!("{}:{}: {}", self.file, self.line, self.text)
    }
}

/// Scan `root`'s production sources (cleaned: strings blanked, comments
/// stripped, test modules region-skipped), collecting every CLEANED line
/// matching `pattern`. Findings report the original line for display.
fn scan(root: &Path, pattern: &dyn Fn(&str) -> bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    visit_rs(root, &mut |path, text| {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let original: Vec<&str> = text.lines().collect();
        for (i, line) in clean_source(text).into_iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if pattern(line.as_str()) {
                findings.push(Finding {
                    file: rel.clone(),
                    line: i + 1,
                    text: original[i].trim().to_string(),
                });
            }
        }
    });
    findings
}

/// True when `line` calls `name(…)` as a FREE function: every occurrence of
/// `name(` whose immediately preceding character is not a `.` — a `.name(`
/// is a METHOD call (e.g. the backends' `command.stdout(Stdio::…)` setter,
/// which aims the CHILD's stdio and is a sanctioned, different shape), while
/// `std::io::stderr(` and a bare imported `stdout(` are the process-stdio
/// free calls this audit counts.
fn free_call(line: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(rel) = line[from..].find(&needle) {
        let start = from + rel;
        if start == 0 || bytes[start - 1] != b'.' {
            return true;
        }
        from = start + 1;
    }
    false
}

#[test]
fn the_engine_never_reads_stdin_prints_prompts_or_installs_global_process_state() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    // ---- No stdin reads: the engine never touches the process's standard
    // input (`io::stdin` covers std + tokio; the spawned CHILD's stdin pipe —
    // `send_input`, the backend's `has_stdin` — is a different, legitimate
    // surface and does not match). ----
    let stdin = scan(&src, &|line| line.contains("io::stdin"));
    assert!(
        stdin.is_empty(),
        "the engine must never read process stdin (no TTY, no prompts): {}",
        stdin
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // ---- No terminal DETECTION: an engine that branches on terminal-ness
    // violates no-TTY without ever printing a prompt, so the TTY-detection
    // vocabulary is barred outright. ----
    let tty = scan(&src, &|line| {
        [
            "IsTerminal",
            "is_terminal",
            "isatty",
            "atty",
            "termios",
            "tcgetattr",
        ]
        .iter()
        .any(|p| line.contains(p))
    });
    assert!(
        tty.is_empty(),
        "the engine must never detect or branch on terminal-ness: {}",
        tty.iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // ---- No interactive prompts: no `println!`/`eprintln!`/`print!`/`dbg!`,
    // and no `writeln!`/`write!` aimed at stdio, ANYWHERE in production
    // sources. Story 10-2 CLOSED the audit's two historical findings: the
    // DC-10 memory-delivery notice and the enforcement breadcrumb (both
    // citing spine AD-12) now route through the host-provided diagnostic sink
    // (`Supervisor::emit_diagnostic` — stderr survives only as the no-sink
    // DEFAULT, which the `diagnostic_sink.rs` suite proves byte-identical).
    // The print-site allowlist is therefore EMPTY: any NEW raw print in
    // production sources fails here outright. Hardened (Epic-10 review)
    // against the imported-path bypass: a hand-written `_print` (the
    // println!-family's expansion internals) fails outright, and the
    // write-target check matches BARE call forms (`stdout(`/`stderr(`), not
    // just the fully-qualified ones — `use std::io::stdout;` + a bare
    // `stdout().write_all(...)` cannot evade by shortening the path. The
    // zero-scan's blind spot left (a write! into a locally-bound handle
    // variable) is covered by the count pin below, which matches the
    // IMPORTS too. TEETH beyond the zero-scan: the sink plumbing is
    // positively pinned below, so the choke point cannot silently disappear
    // or lose a diagnostic. ----
    let prompts = scan(&src, &|line| {
        ["println!", "eprintln!", "print!", "dbg!"]
            .iter()
            .any(|p| line.contains(p))
            || line.contains("_print")
            || ((line.contains("writeln!(") || line.contains("write!("))
                && (line.contains("io::stderr")
                    || line.contains("io::stdout")
                    || line.contains("stderr(")
                    || line.contains("stdout(")))
    });
    assert!(
        prompts.is_empty(),
        "a raw print site appeared in production sources — the engine's ONLY \
         diagnostic emission is the story-10-2 diagnostic sink \
         (`Supervisor::emit_diagnostic` in domain/supervisor.rs; stderr is its \
         no-sink default). A print is a control surface: route it through the \
         sink or re-review the sink contract: {}",
        prompts
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // ---- Positive story-10-2 sink pins (the retired allowlist's teeth, kept
    // in the new shape): each fragment must match EXACTLY ONE site, so a
    // removed or reworded diagnostic route — or a second direct stderr writer
    // sneaking past the print scan above — fails here. ----
    let sink_pins: [(&str, &str, &str); 11] = [
        // The ONE diagnostic emission choke point (every pinned diagnostic routes
        // through it; the `[ktesio] ` prefix + terminating newline live here).
        (
            "domain/supervisor.rs",
            "fn emit_diagnostic(&self",
            "the diagnostic-sink choke point",
        ),
        // The DC-10 memory-delivery notice's route into the sink (story
        // 5-1/2-2 Decision 6 — the operator attach action is owed the truth).
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&notice)",
            "the DC-10 memory-delivery notice's route into the sink",
        ),
        // The enforcement breadcrumb's route into the sink (a breach action
        // that could not be honored, or a breach record that failed to append
        // — an operator breadcrumb, never the record). The marker is the
        // whole single-line call, so it stays distinct from the AI-41 site
        // below (a multi-line `format!` whose call line carries no args).
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&format!(",
            "the enforcement breadcrumb's route into the sink",
        ),
        // AI-41 (story 11-1): a usage event whose ledger INSERT failed — the
        // event was NOT counted and the drain will retry it. A NEW third
        // diagnostic route, routed through the same `emit_diagnostic` choke
        // point; the count was consciously re-reviewed and this site pinned
        // by its own route line (distinct from the enforcement breadcrumb's
        // single-line `format!` call above).
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&failure);",
            "the AI-41 usage-commit-failure report's route into the sink",
        ),
        // AI-9 (story 11-1, loop 1): the SIGNAL-FAILURE breadcrumb — the
        // transition committed but the pause/resume signal failed, so the
        // ledger and the live process may diverge; the divergence is
        // announced, never silent.
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&signal_failure);",
            "the AI-9 signal-failure divergence breadcrumb's route into the sink",
        ),
        // AI-12 (story 11-1, loop 1): the ENVIRONMENTAL poll-failure notice —
        // multiple handles failing in the same tick corroborate as a
        // backend/environment-wide condition; the no-mass-crash guard's
        // decision is announced, never silent.
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&environmental);",
            "the AI-12 environmental poll-failure notice's route into the sink",
        ),
        // AI-41 (story 11-1, loop 1): the TERMINAL-drain loss notice — a
        // still-failing INSERT on the final drain cannot be retried (the
        // handle is being removed), so the loss is announced, never silent.
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&loss);",
            "the AI-41 terminal-drain loss notice's route into the sink",
        ),
        // AI-46 (story 11-3): the STRANDED-LISTENER adoption notice — an
        // adopted engine-observed orphan whose injected `base_url` still
        // points at the PREVIOUS engine's dead listener; the condition and the
        // stop→start remediation are announced at adoption, never silent.
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&strand);",
            "the AI-46 stranded-listener adoption notice's route into the sink",
        ),
        // AI-27 (story 11-2): the ENV-SHADOW notice — a config-mapped env
        // target overwrote a same-named `[lifecycle.start]` base-launch env
        // var; the config value wins (precedence untouched) and the shadow is
        // announced at start, never silent.
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&shadow_notice);",
            "the AI-27 env-shadow notice's route into the sink",
        ),
        // AI-39 (story 11-2): the SECRET-INTO-FLAG runtime notice — a resolved
        // `secret:` leaf was delivered into a FLAG target (cleartext on argv,
        // the accepted-but-stricter boundary); the fact is announced warn-only
        // on every start that delivers one, never silent, never the value.
        (
            "domain/supervisor.rs",
            "self.emit_diagnostic(&flag_notice);",
            "the AI-39 secret-into-flag notice's route into the sink",
        ),
        // The stderr DEFAULT arm: with no sink installed the diagnostics go
        // to stderr byte-identically to the pre-sink engine (pinned
        // end-to-end by the `diagnostic_sink.rs` subprocess suite).
        (
            "domain/supervisor.rs",
            "std::io::stderr().write_all(",
            "the sink's stderr default arm",
        ),
    ];
    for (file, marker, what) in sink_pins {
        let hits: Vec<String> = scan(&src, &|line| line.contains(marker))
            .iter()
            .filter(|f| f.file == file && f.text.contains(marker))
            .map(Finding::describe)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "the story-10-2 sink pin for {what} ({file}, {marker}) must match \
             EXACTLY ONE site — 0 means the diagnostic disappeared or its \
             route was reworded, >1 means a duplicate appeared; both require \
             re-reviewing the sink contract: {hits:?}"
        );
    }
    // The stderr default arm is the ONLY place the engine touches stdio at
    // all — a second stdio reach anywhere in the production sources is
    // exactly the uninvited-write class story 10-2 closed. (Run crate-wide,
    // not per-file: the pin above keeps the sanctioned site honest, this
    // keeps the COUNT honest.) Widened (Epic-10 review) so a stdio write via
    // an IMPORTED path cannot evade the count the fully-qualified forms
    // satisfy: the classes are the call forms — fully qualified
    // (`std::io::stderr()`) AND bare (`use std::io::stdout;` then
    // `stdout().write_all(...)`; the zero-arg handle constructor always
    // carries the `()`) — the import forms (`use std::io::stdout;` or a
    // brace-group `use std::io::{stdout, …}`), and a hand-written `_print`
    // (println!/print!'s expansion internals). The backends' CHILD-stdio
    // setters (`command.stdout(Stdio::…)`) are DOT-prefixed METHOD calls —
    // a different, sanctioned shape ([`free_call`] excludes them) aiming the
    // spawned child's stdio, never the host process's.
    let stdio_reaches = scan(&src, &|line| {
        free_call(line, "stdout")
            || free_call(line, "stderr")
            || (line.contains("use std::io")
                && (line.contains("stdout") || line.contains("stderr")))
            || line.contains("_print")
    });
    assert_eq!(
        stdio_reaches.len(),
        1,
        "the engine must reach stdio in EXACTLY ONE place (the diagnostic \
         sink's stderr default arm in domain/supervisor.rs) — counted across \
         fully-qualified calls, bare imported-path calls, stdio imports, and \
         hand-written _print internals: {}",
        stdio_reaches
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
    assert_eq!(
        stdio_reaches[0].file, "domain/supervisor.rs",
        "the one stdio reach must be the sink's stderr default arm"
    );
    assert!(
        stdio_reaches[0].text.contains("stderr"),
        "the one stdio reach must be the STDERR default arm, not stdout: {}",
        stdio_reaches[0].describe()
    );

    // ---- No env MUTATION: the engine reads `KTESIO_STATE_DIR` etc. (fine);
    // it must never `set_var`/`remove_var` at engine runtime (the env
    // mutations in paths.rs/secret_resolver.rs are #[cfg(test)]-only, which
    // the region-skipping scan excludes). ----
    let env_mutation = scan(&src, &|line| {
        line.contains("set_var") || line.contains("remove_var")
    });
    assert!(
        env_mutation.is_empty(),
        "the engine must never mutate the Host's process environment: {}",
        env_mutation
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // ---- No process-global handler installs: no signal hooks (including
    // `tokio::signal`, which registers a real process-global OS handler, and
    // the raw `signal(` C entry point), no panic hooks, no console control /
    // unhandled-exception / vectored-exception handlers, no `atexit`. The
    // backends SEND signals to child process groups — they never INSTALL
    // handlers in the Host's process. ----
    let handlers = scan(&src, &|line| {
        [
            "signal_hook",
            "ctrlc",
            "ctrl_c",
            "sigaction",
            "SetConsoleCtrlHandler",
            "SetUnhandledExceptionFilter",
            "AddVectoredExceptionHandler",
            "set_hook",
            "libc::signal",
            "signal(",
            "tokio::signal",
            "SignalKind",
            "atexit",
        ]
        .iter()
        .any(|p| line.contains(p))
    });
    assert!(
        handlers.is_empty(),
        "the engine must never install a process-global handler: {}",
        handlers
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // ---- No lazy/global cells: no `lazy_static`/`once_cell`/`LazyLock`/
    // `LazyCell`/`OnceLock`/`OnceCell`/`thread_local!`/`static mut` anywhere
    // in production sources. The ONE named-allowlisted global is `RUN_NONCE`
    // (a plain AtomicU64 static, asserted separately below). ----
    let global_cells = scan(&src, &|line| {
        [
            "lazy_static",
            "once_cell",
            "LazyLock",
            "LazyCell",
            "OnceLock",
            "OnceCell",
            "thread_local!",
            "static mut",
        ]
        .iter()
        .any(|p| line.contains(p))
    });
    assert!(
        global_cells.is_empty(),
        "the engine must hold no lazy/global cell: {}",
        global_cells
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // ---- The NAMED allowlist of process-global statics. TEETH: EVERY static
    // carrying a lock or atomic — `pub` or private — must be one of these
    // named sites, so a new `pub static FOO: AtomicU64` cannot ride a
    // private-only exclusion. Each entry is the same reviewed class: a plain
    // atomic counter, never read for behavior, never engine-coupled,
    // guaranteeing PER-PROCESS uniqueness only (cross-process uniqueness is
    // not claimed — separate pids / separate roots keep separate state).
    //
    // 1. `static RUN_NONCE: AtomicU64` (domain/usage.rs) — the RunId
    //    uniqueness tie-breaker, consulted ONLY inside `RunId::mint` to keep
    //    two same-nanosecond Run ids distinct (the collision test's
    //    disjoint-run-id assertion proves the property).
    // 2. `static TEMP_WRITE_SEQ: AtomicU64` (paths.rs, review-1 patch 1) —
    //    the atomic-write temp-name disambiguator, consulted ONLY inside
    //    `paths::reserve_atomic_temp` so two THREADS of one process writing
    //    the same target concurrently cannot collide on a temp path; the pid
    //    in the same name carries the cross-process half. ----
    let statics = scan(&src, &|line| {
        line.contains("static ")
            && (line.contains("Atomic") || line.contains("Mutex") || line.contains("RwLock"))
    });
    assert_eq!(
        statics.len(),
        2,
        "exactly TWO named process-global statics are permitted (RUN_NONCE + \
         TEMP_WRITE_SEQ); a new static — `pub` or private — is exactly the \
         collision class FR-34 forbids and requires re-reviewing this \
         allowlist: {}",
        statics
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
    let mut homes: Vec<(&String, &String)> = statics.iter().map(|f| (&f.file, &f.text)).collect();
    homes.sort();
    assert_eq!(
        homes.len(),
        2,
        "expected exactly two allowlisted statics after sorting"
    );
    let (usage, paths) = (&homes[0], &homes[1]);
    assert_eq!(usage.0, "domain/usage.rs", "the RUN_NONCE static's home");
    assert!(
        usage.1.contains("RUN_NONCE") && usage.1.contains("AtomicU64"),
        "the usage.rs static must still be the RUN_NONCE counter (a shape change \
         requires re-reviewing this allowlist): {}",
        usage.1
    );
    assert_eq!(paths.0, "paths.rs", "the TEMP_WRITE_SEQ static's home");
    assert!(
        paths.1.contains("TEMP_WRITE_SEQ") && paths.1.contains("AtomicU64"),
        "the paths.rs static must still be the TEMP_WRITE_SEQ counter (a shape change \
         requires re-reviewing this allowlist): {}",
        paths.1
    );

    // ---- No ambient/global runtime: a tokio runtime is built ONLY inside
    // engine.rs (the per-engine runtime `Engine::open` owns and aborts with
    // the engine). No other production file may create one (every other
    // construction — the listener's test runtime included — is
    // #[cfg(test)]-only, excluded by the region-skipping scan), and nothing
    // may reach for an ambient handle, `current` OR `try_current` (the
    // supervisor is handed the engine's handle explicitly). ----
    let runtimes = scan(&src, &|line| {
        line.contains("Runtime::new") || line.contains("runtime::Builder")
    });
    assert_eq!(
        runtimes.len(),
        1,
        "exactly ONE runtime construction site is permitted (Engine::open's owned \
         runtime): {}",
        runtimes
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
    assert_eq!(
        runtimes[0].file, "engine.rs",
        "the runtime must be built by the Engine itself, per engine"
    );
    let ambient = scan(&src, &|line| {
        line.contains("Handle::current") || line.contains("Handle::try_current")
    });
    assert!(
        ambient.is_empty(),
        "the engine must never grab an ambient runtime handle: {}",
        ambient
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
}

/// Parse a method inventory from a cleaned source region: every `marker NAME`
/// occurrence maps to its whitespace-normalized PARAMETER LIST (extracted by
/// paren-depth scanning, so multi-line signatures are captured whole). The
/// normalization makes the name-AND-signature comparison whitespace-insensitive.
fn inventory_map(region: &str, marker: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut from = 0;
    while let Some(rel) = region[from..].find(marker) {
        let start = from + rel;
        let after = &region[start + marker.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let params = match after.find('(') {
            Some(open) => {
                let bytes = after.as_bytes();
                let mut depth = 0i32;
                let mut end = open;
                for (k, &b) in bytes.iter().enumerate().skip(open) {
                    match b {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = k;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                after[open + 1..end].to_string()
            }
            None => String::new(),
        };
        // Normalize: whitespace-insensitive AND trailing-comma insensitive
        // (a multi-line signature formats a trailing comma a single-line one
        // omits; the SEGMENTS are what must match).
        let normalized = params
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        map.insert(name, normalized);
        from = start + marker.len();
    }
    map
}

#[test]
fn every_public_async_engine_entry_point_has_a_blocking_facade_counterpart() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    // ---- WHOLE-CRATE async enumeration: every `pub async fn` in ANY
    // production module (cleaned; test modules region-skipped) is an entry
    // point the facade must cover — an `impl Engine { pub async fn … }`
    // added to another file cannot escape the count. (Today every
    // `pub async fn` lives on `Engine`; a future one elsewhere fails here
    // until it is consciously covered or the audit is re-scoped.) ----
    let mut crate_async: BTreeMap<String, String> = BTreeMap::new();
    visit_rs(&src, &mut |_path, text| {
        for line in clean_source(text) {
            if let Some(rest) = line.trim_start().strip_prefix("pub async fn ") {
                if let Some(name) = rest.split('(').next() {
                    crate_async.insert(name.trim().to_string(), String::new());
                }
            }
        }
    });

    // The engine.rs regions, parsed from CLEANED text (comments and strings
    // stripped — the same hygiene the scanner uses, so the inventory and the
    // block_on bridge check can never disagree about what is code). Each
    // region marker must occur EXACTLY ONCE before any slicing.
    let engine_rs =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine.rs"))
            .expect("read engine.rs");
    let engine_lines = clean_source(&engine_rs);
    let engine_text = engine_lines.join("\n");
    assert_eq!(
        engine_text.matches("impl Engine {").count(),
        1,
        "the impl Engine marker must be unique for the region slice to be sound"
    );
    assert_eq!(
        engine_text.matches("impl Blocking<'_> {").count(),
        1,
        "the impl Blocking marker must be unique for the region slice to be sound"
    );
    let engine_start = engine_text
        .find("impl Engine {")
        .expect("the Engine impl exists");
    let facade_start = engine_text
        .find("impl Blocking<'_> {")
        .expect("the Blocking impl exists");
    let facade_end = engine_text[facade_start..]
        .find("pub struct EventSubscription")
        .map(|i| facade_start + i)
        .expect("EventSubscription follows the Blocking impl");
    let async_region = &engine_text[engine_start..facade_start];
    let facade_region = &engine_text[facade_start..facade_end];

    let engine_async = inventory_map(async_region, "pub async fn ");
    // The whole-crate parse is authoritative; the region parse must never
    // find something the crate parse missed (marker rot detector).
    assert!(
        !engine_async.is_empty(),
        "the impl-Engine region parse must find the async inventory"
    );
    for name in engine_async.keys() {
        assert!(
            crate_async.contains_key(name),
            "`Engine::{name}` was found in the impl-Engine region but not crate-wide — \
             the whole-crate enumeration regressed"
        );
    }
    // Parsing sanity: a silent under-parse must not vacuously pass.
    assert!(
        crate_async.len() >= 20,
        "the whole-crate async inventory parse regressed (got {}): {crate_async:?}",
        crate_async.len()
    );

    let facade = inventory_map(facade_region, "pub fn ");

    // FR-34: the facade covers the FULL async API — no async entry point
    // without a sync counterpart.
    let missing: Vec<&String> = crate_async
        .keys()
        .filter(|n| !facade.contains_key(*n))
        .collect();
    assert!(
        missing.is_empty(),
        "async entry points without a Blocking facade counterpart (a Host on the \
         sync surface loses them): {missing:?}"
    );

    // The facade's only intentional EXTRAS are the two sync-side methods —
    // the bridged `subscribe` (the sync EventSubscription over the sync
    // Engine::subscribe) and the story-10-2 sink installer `with_diagnostics`
    // (a lock-and-field-swap over Engine::with_diagnostics; neither has — or
    // needs — an async form). Anything else means the two surfaces drifted
    // and must be reviewed.
    let extras: Vec<&String> = facade
        .keys()
        .filter(|n| !crate_async.contains_key(*n))
        .collect();
    assert_eq!(
        extras,
        ["subscribe", "with_diagnostics"],
        "the facade's only extras must be the bridged subscribe and the \
         story-10-2 sink installer with_diagnostics: {extras:?}"
    );

    // Name AND signature: each counterpart's parameter list must equal the
    // async method's (whitespace-normalized), so an arity/type change cannot
    // silently diverge the two surfaces.
    for (name, async_params) in &engine_async {
        let facade_params = facade
            .get(name)
            .unwrap_or_else(|| panic!("`{name}` missing from the facade inventory"));
        assert_eq!(
            async_params, facade_params,
            "Blocking::{name}'s signature drifted from Engine::{name} — the facade must \
             mirror the async parameter list exactly (FR-34 name-and-signature)"
        );
    }

    // Each counterpart really bridges through the engine runtime
    // (`block_on`) — not a reimplementation (parsed over the CLEANED region,
    // the same hygiene as the inventory). `subscribe` constructs the bridged
    // EventSubscription instead, and `with_diagnostics` is the story-10-2
    // sync sink installer (both covered by the extras assertion above).
    for chunk in facade_region.split("pub fn ").skip(1) {
        let name = chunk.split('(').next().unwrap_or("").trim();
        if name == "subscribe" || name == "with_diagnostics" || name.is_empty() {
            continue;
        }
        assert!(
            chunk.contains("block_on"),
            "Blocking::{name} must bridge via block_on, not reimplement the async method"
        );
    }
}

#[test]
fn kt_consumes_the_engine_only_through_the_blocking_facade() {
    let kt_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kt/src")
        .canonicalize()
        .expect("kt's src tree sits beside the engine crate in the workspace");

    // kt is a synchronous binary: it structurally cannot await an engine
    // future. This audit pins that shape at the source level — the denylist
    // runs on CLEANED code lines (comments and string literals cannot match),
    // so a doc mention or a log message can never false-positive (or mask a
    // real use). The build-level boundary proof is story 7-4's; this is the
    // source-level companion.
    let async_use = scan(&kt_src, &|line| {
        [
            ".await",
            "async fn",
            "async move",
            "async block",
            "async {",
            "async_trait",
            "futures::",
            "now_or_never",
            "tokio::",
            "block_on",
            "Runtime::new",
        ]
        .iter()
        .any(|p| line.contains(p))
    });
    assert!(
        async_use.is_empty(),
        "kt must never touch an engine async API or a runtime directly (it drives \
         the blocking facade only): {}",
        async_use
            .iter()
            .map(Finding::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );

    // And POSITIVELY: kt really drives the facade (the sanctioned embedding
    // surface) — `.blocking()` reachable on a CODE line (string-literal
    // contents are blanked before this matches).
    let facade_use = scan(&kt_src, &|line| line.contains(".blocking()"));
    assert!(
        !facade_use.is_empty(),
        "kt's engine consumption must go through .blocking() — none found"
    );

    // The "no tokio dependency" claim, pinned structurally: kt's manifest
    // declares no tokio edge (TOML `#` comments excluded), so the denylist
    // above has a build-level backstop inside this same audit.
    let manifest =
        std::fs::read_to_string(kt_src.parent().expect("kt crate root").join("Cargo.toml"))
            .expect("read kt/Cargo.toml");
    let tokio_refs: Vec<&str> = manifest
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && l.contains("tokio")
        })
        .collect();
    assert!(
        tokio_refs.is_empty(),
        "kt must not declare a tokio dependency (the facade is the only runtime \
         bridge): {tokio_refs:?}"
    );
}

#[test]
fn the_memory_attach_json_readback_error_arm_routes_to_attach_readback_failed() {
    let kt_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kt/src")
        .canonicalize()
        .expect("kt's src tree sits beside the engine crate in the workspace");

    // Retro #166 finding B10's operator contract, pinned at the SOURCE level
    // (the embed-clean audit's fragment-pin pattern: a UNIQUE fragment that
    // must match EXACTLY ONE site). The `memory attach --json` read-back Err
    // arm must route to `attach_readback_failed` — the composer that names
    // the PERSISTED attachment (the backing remains attached) and both
    // remediations. Only the composer is unit-tested, so reverting the arm
    // to a generic error map would pass every existing test while silently
    // losing the contract; this pin closes that gap. 0 hits = the arm was
    // reverted or reworded; >1 = a duplicate appeared; both require
    // re-review. Matching runs on string-cleaned, comment-stripped code
    // lines, so a doc mention or a log message can never satisfy it.
    let arm = scan(&kt_src, &|line| {
        line.contains("Err(err) => return Err(attach_readback_failed(name, kind, &err).into())")
    });
    assert_eq!(
        arm.len(),
        1,
        "the memory attach --json read-back Err arm must route through \
         attach_readback_failed at EXACTLY ONE site — 0 means the arm was \
         reverted or reworded (the persisted-attachment contract is lost), \
         >1 means a duplicate appeared; both require re-review: {:?}",
        arm.iter().map(Finding::describe).collect::<Vec<_>>()
    );
    assert!(
        arm[0].file.ends_with("cli/agent.rs"),
        "the arm lives in kt's agent command surface: {}",
        arm[0].describe()
    );
}
