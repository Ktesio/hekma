//! Story 7-1 (FR-31): the LIBRARY HOST test — a host that links only
//! `hemaka-engine`'s public API drives the FULL UJ-3 flow
//! register → configure → cap → start → breach → pause → stop with no `kt`
//! and no CLI, asserting every step against the expectations pinned ONCE in
//! `hemaka_conformance::uj3` — the same module the CLI journey test
//! (`crates/kt/tests/agent_cli.rs`) consumes, which is what proves the library
//! path and the CLI path are behaviorally identical.
//!
//! ## Boundary (what drives what)
//!
//! Every state change and every read below goes through [`Engine::open`] +
//! `blocking()` — the sanctioned embedding surface (AD-13), the same surface
//! `kt` itself drives. `hemaka_conformance` is used as TEST INFRASTRUCTURE
//! ONLY (the shared expectations fixture + the committed-state readers): it is
//! a dev-dependency of this suite, never a driving surface, and dev-deps don't
//! cross the shipping boundary gate (`cargo tree -p ktesio -e normal,build`
//! stays clean). The flow's agent is the conformance `fake_agent` fixture —
//! the same proven `budget.rs`/`cost.rs` emission pattern (fixed token
//! sentinels, known event count) — so the shared numbers are exact.
//!
//! ## Reachability inventory (the §4.x capability map, per the frozen spec)
//!
//! * **§4.1 Agent Registration & Fleet** — EXERCISED: manifest registration
//!   (`register_with_adapter`) + the Fleet reads before the flow, after the
//!   breach, and after the stop.
//! * **§4.2 Unified Lifecycle** — EXERCISED: `start`, the breach-driven
//!   `running → paused` with its `BudgetExceeded` cause, and `stop` to the
//!   terminal state.
//! * **§4.3 Unified Configuration** — EXERCISED: `set_config` for every flow
//!   key (the shared pairs) + the `effective_config` read with per-leaf
//!   provenance.
//! * **§4.4 Memory Wiring** — cited, NOT re-tested: its facade reachability is
//!   already proven by the engine's hermes e2e (`tests/hermes.rs` — Phase B
//!   attaches a `filesystem` backing and proves the `HERMES_HOME` delivery
//!   through the same `Blocking` facade; Phase K attaches `native`), so the
//!   flow here does not duplicate it.
//! * **§4.5 Token & Cost Governance** — EXERCISED: the token budget + Rate +
//!   dollar cap armed through config, the breach records (exactly one per
//!   dimension — both armed ceilings cross on the same event; the token
//!   breach wins the pause), and the honest labeled dollar surfaces on every
//!   read.
//! * **§4.6 Unified Interaction** — cited, NOT re-tested: `send_input`'s
//!   facade reachability is already proven by the hermes e2e (Phase C's
//!   `send_input` round-trip; Phase L re-proves interaction on the governed
//!   instance after the whole governance journey).
//! * **§4.7 Adapter Contract & Reference Adapter** — EXERCISED by
//!   construction: the flow registers through a `contract_version = "1.0.0"`
//!   manifest (the contract registration gate negotiates it) carrying the
//!   fixture's per-OS Capability Declaration and Metering Source.
//! * **§4.8 Embeddable Engine** — THIS test is its §4.8/FR-31 discharge: the
//!   whole flow above runs through the Embedding Interface (`Engine::open` +
//!   the `blocking()` facade) with no CLI and no private items, proving every
//!   capability the flow touches is reachable library-alone and behaviorally
//!   identical to the documented `kt` path (the CLI journey in
//!   `crates/kt/tests/agent_cli.rs` feeds the SAME shared assertions).
//! * **§4.9 Agent-Scoped Skills Provisioning** — out of scope (epic 8).
//! * **§4.10 Migration & Deprecation** — out of scope (the legacy-surface
//!   story is epic 8/9 territory; nothing here touches migration).
//!
//! ## Determinism (the budget.rs posture, shared)
//!
//! No wall-clock sleeps against side effects: the fixture emits a KNOWN number
//! of usage events with FIXED token sentinels, the evaluator runs
//! synchronously inside the ingestion path, and every wait polls the COMMITTED
//! state (`uj3::wait_for_state`, the same reader both suites share). The
//! manifest declares pause `guaranteed` on all three OSes, so the default
//! pause Breach Action lands the committed `paused` state deterministically
//! everywhere — which is what the assertions pin (no `OsId` gate anywhere).
//! HONESTY about the suspension itself: on Unix it is a real SIGSTOP freeze;
//! on Windows it is cooperative best-effort with no hard suspension — exactly
//! why the usage assertions are committed RANGES, not exact counts. All
//! timing comes from the shared module's constants (poll budget, stop
//! window) — never an inline restatement. One honesty note: the POST-STOP
//! ledger is a committed RANGE, not an exact total — the breach's suspension
//! can freeze the emitter at event 3, 4, or 5 on Unix (and on Windows the
//! cooperative pause simply lets the batch finish) — see
//! `uj3::assert_stopped_usage` for the exact shape of the range.

use hemaka_conformance::uj3;
use hemaka_engine::{AdapterRef, ConfigLayer, Engine, LifecycleState};
use tempfile::TempDir;

#[test]
fn the_library_host_drives_the_full_uj3_flow_through_the_blocking_facade_alone() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // §4.7: the fixture manifest — contract v1, per-OS capability declaration,
    // self-reported metering, and the `model` → env mapping that makes the
    // configure leg meaningful. Authored by the SHARED module (pinned once).
    let manifest_dir = uj3::write_flow_manifest(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // ---- §4.1 register: a manifest adapter, library-only. ----
    facade
        .register_with_adapter(uj3::FLOW_INSTANCE, &AdapterRef::Manifest(manifest_dir))
        .expect("register the flow's agent");

    // ---- §4.3 + §4.5 configure: the SHARED key/value pairs, written through
    // the facade one set_config call at a time (the same writes the CLI
    // journey performs through documented `kt agent config set`). ----
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(uj3::FLOW_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }

    // ---- §4.3 read back: the effective config carries the configured value
    // at its provenance layer. The SAME shared assertion the CLI journey feeds
    // from its `config get --json` leaf. ----
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

    // ---- §4.1 Fleet read, pre-start: the seeded budget + honest zero usage.
    // The SAME shared assertion the CLI journey feeds from `show --json`. ----
    let entry = fleet_entry(&facade);
    uj3::assert_pre_start_entry(&entry);

    // ---- §4.2 start. The instance is inserted into supervision before this
    // returns, so the reaper's first post-start tick ingests the (already
    // emitted) batch and the breach fires deterministically. ----
    facade
        .start(uj3::FLOW_INSTANCE)
        .expect("start the flow's agent");

    // ---- §4.5 breach → pause, polled against COMMITTED state (never a
    // wall-clock guess). ----
    uj3::wait_for_state(
        state.path(),
        uj3::FLOW_INSTANCE,
        LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );

    // The enforcement chain's event half: usage event → BudgetEvaluator →
    // exactly ONE breach PER DIMENSION (both armed ceilings cross on the same
    // event; the independent latches each fire once), asserted through the
    // facade read AND through the shared committed-log reader — the reader the
    // CLI journey depends on reads the SAME records the facade returns.
    let breaches = facade
        .budget_breach_events(uj3::FLOW_INSTANCE)
        .expect("breach events read");
    uj3::assert_flow_breaches(&breaches);
    let from_log = uj3::read_breach_events(state.path(), uj3::FLOW_INSTANCE);
    uj3::assert_flow_breaches(&from_log);

    // The enforcement chain's lifecycle half: the `running → paused`
    // transition carries the TOKEN `BudgetExceeded` cause — the token breach
    // won the pause (the dollar breach found the instance already paused and
    // recorded only).
    let transitions = facade
        .transition_events(uj3::FLOW_INSTANCE)
        .expect("transition events read");
    uj3::assert_paused_transition_budget_exceeded(&transitions);

    // The honest post-breach readback (fleet/usage surfaces report truthfully:
    // paused, saturated remainders, labeled dollars equal to the tokens).
    let entry = fleet_entry(&facade);
    uj3::assert_paused_entry(&entry);

    // ---- §4.2 stop: the flow's last leg, back through the facade — with the
    // SHARED zero stop window (the breach suspended the process; it cannot act
    // on a graceful signal, so the window would only elapse). The SAME window
    // the CLI journey's `--timeout` passes. ----
    facade
        .stop(uj3::FLOW_INSTANCE, Some(uj3::STOP_WINDOW))
        .expect("stop the flow's agent");

    // The frozen ledger (a committed RANGE — the suspension races the emitter;
    // see the module docs) plus the terminal state and honest surfaces hold.
    // The SAME shared assertion the CLI journey feeds from its post-stop
    // `show --json`.
    let entry = fleet_entry(&facade);
    uj3::assert_stopped_entry(&entry);
}

/// The Fleet detail row for the flow's instance, through the facade read.
fn fleet_entry(facade: &hemaka_engine::Blocking<'_>) -> hemaka_engine::FleetEntry {
    facade
        .fleet()
        .expect("fleet read")
        .into_iter()
        .find(|e| e.name.as_str() == uj3::FLOW_INSTANCE)
        .expect("the flow's instance is in the Fleet")
}
