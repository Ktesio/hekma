//! The Policy money tests, driven end to end (story 21-3, the adversarial
//! review round 1 finding 1): USAGE under a NAMED Policy enforces — the
//! breach fires from a usage event (not only from a Policy change), the
//! action applies to the supervised process, a Policy's flat Rate prices
//! the ledger with no instance Rate anywhere, and the Fleet view names the
//! Policy's limit and source. The child-Run case (a child bound into a
//! Policy breaching from its own usage) lives in the supervisor suite,
//! which holds the attributed-usage seam.
//!
//! Same posture as `tests/budget.rs`: the REAL `fake_agent` emits known
//! usage (10 in / 20 out = 30 tokens per event) through the public
//! [`Engine`]; every assertion is on committed state (the breach rows, the
//! lifecycle state, the ledger's priced totals), never on timing.

use std::path::Path;
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, BreachAction, BreachDimension, BreachScope, Engine, LifecycleState, PolicyChange,
    PolicyLimits, Rate,
};
use tempfile::TempDir;

/// 10 in + 20 out tokens per `fake_agent --emit-usage` event.
const TOKENS_PER_EVENT: u64 = 30;

fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
    hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction")
        .guaranteed_on_all_oses("pause")
        .write(dir);
}

fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// Poll the committed lifecycle state for `name` until it equals `want`.
fn wait_for_state(state_dir: &Path, name: &str, want: LifecycleState, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let conn = rusqlite::Connection::open(state_dir.join("state.db")).ok();
        let state = conn.and_then(|conn| {
            conn.query_row(
                "SELECT state FROM agent_instances WHERE name = ?1",
                [name],
                |r| r.get::<_, String>(0),
            )
            .ok()
        });
        if state.as_deref() == Some(want.as_str()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for '{name}' to reach {} (committed state: {state:?})",
            want.as_str()
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// Register `name` (to emit `events` usage events once started), with its
/// configuration binding the `team=core` label its Run will carry, and the
/// `cap` Policy over that label ALREADY in place — usage must flow UNDER
/// the Policy (the ledger prices each row at its commit-time Rate; a Policy
/// created after the events would face an unpriced ledger, honestly
/// unbreachable).
fn fleet_under(name: &str, events: u32, limits: PolicyLimits) -> (TempDir, Engine) {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(
        manifest.path(),
        name,
        &["--emit-usage", &events.to_string(), "--linger-ms", "600000"],
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.set_config(name, "labels.team", "core").unwrap();
    facade
        .create_policy(
            PolicyChange {
                name: "cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: limits,
                clear: Vec::new(),
                rates: Default::default(),
            },
            false,
        )
        .expect("the Policy change applies");
    facade.start(name).unwrap();
    (state, engine)
}

fn one_breach(engine: &Engine, name: &str) -> hekma_engine::BudgetBreachEvent {
    let breaches = engine.blocking().budget_breach_events(name).unwrap();
    assert_eq!(breaches.len(), 1, "{breaches:?}");
    breaches[0].clone()
}

#[test]
fn usage_under_a_named_policy_enforces_the_token_scopes_and_pauses() {
    // Tokens, per-run AND cumulative, under a NAMED Policy (nothing on the
    // instance): the breach fires from the USAGE event, `pause` applies to
    // the supervised process, and the breach row carries the Policy's
    // ceiling. 5 events × 30 = 150 tokens.
    for (limits, scope, ceiling) in [
        (
            PolicyLimits {
                tokens_per_run: Some(100),
                breach_action: Some(BreachAction::Pause),
                ..PolicyLimits::default()
            },
            BreachScope::PerRun,
            100,
        ),
        (
            PolicyLimits {
                tokens_cumulative: Some(100),
                breach_action: Some(BreachAction::Pause),
                ..PolicyLimits::default()
            },
            BreachScope::Cumulative,
            100,
        ),
    ] {
        let (state, engine) = fleet_under("polcap", 5, limits.clone());
        wait_for_state(
            state.path(),
            "polcap",
            LifecycleState::Paused,
            Duration::from_secs(30),
        );
        let breach = one_breach(&engine, "polcap");
        assert_eq!(breach.dimension, BreachDimension::Tokens);
        assert_eq!(breach.scope, scope);
        assert_eq!(breach.limit, ceiling);
        assert_eq!(breach.action.as_str(), "pause");
        assert!(breach.observed >= ceiling);
        // The Fleet view names the Policy's limit AND its source.
        let fleet = engine.blocking().fleet().unwrap();
        let entry = fleet.iter().find(|e| e.name.as_str() == "polcap").unwrap();
        let budget = entry.budget.as_ref().expect("the Policy budgets the Run");
        assert_eq!(
            budget.policies,
            vec!["cap".to_string()],
            "the source names the Policy"
        );
        match scope {
            BreachScope::PerRun => assert_eq!(budget.per_run_limit, Some(ceiling)),
            BreachScope::Cumulative => assert_eq!(budget.cumulative_limit, Some(ceiling)),
        }
        assert_eq!(budget.breach_action.as_str(), "pause");
        let _ = engine
            .blocking()
            .stop("polcap", Some(Duration::from_secs(5)));
    }
}

#[test]
fn usage_under_a_named_policy_enforces_the_dollar_scopes_with_its_flat_rate() {
    // Dollars, per-run AND cumulative, under a NAMED Policy that also
    // supplies the flat Rate ($1/$1 per 1M — NO instance Rate anywhere):
    // the Policy's Rate prices the ledger and its Cost Cap breaches from
    // the usage event. 5 events × 30 tokens = 150 tokens = 150 micro-dollars
    // at $1/$1 per 1M → a cap of 100 micro-dollars breaches both scopes.
    for (limits, scope) in [
        (
            PolicyLimits {
                dollars_per_run_micros: Some(100),
                rate: Some(Rate::new(1_000_000, 1_000_000)),
                breach_action: Some(BreachAction::Pause),
                ..PolicyLimits::default()
            },
            BreachScope::PerRun,
        ),
        (
            PolicyLimits {
                dollars_cumulative_micros: Some(100),
                rate: Some(Rate::new(1_000_000, 1_000_000)),
                breach_action: Some(BreachAction::Pause),
                ..PolicyLimits::default()
            },
            BreachScope::Cumulative,
        ),
    ] {
        let (state, engine) = fleet_under("poldlr", 5, limits.clone());
        wait_for_state(
            state.path(),
            "poldlr",
            LifecycleState::Paused,
            Duration::from_secs(30),
        );
        let breach = one_breach(&engine, "poldlr");
        assert_eq!(breach.dimension, BreachDimension::Dollars);
        assert_eq!(breach.scope, scope);
        assert_eq!(breach.limit, 100);
        assert_eq!(breach.action.as_str(), "pause");
        // The Policy's flat Rate priced the ledger (no instance Rate): the
        // derived cost equals the committed tokens × $1/$1 per 1M exactly.
        let fleet = engine.blocking().fleet().unwrap();
        let entry = fleet.iter().find(|e| e.name.as_str() == "poldlr").unwrap();
        let tokens = entry.usage.cumulative_total_tokens();
        assert!(
            tokens >= TOKENS_PER_EVENT * 5,
            "the events committed: {tokens}"
        );
        let cost = entry
            .usage
            .cumulative_dollars
            .expect("the Policy's Rate prices usage");
        assert_eq!(
            cost,
            hekma_engine::Micros(tokens as i64),
            "{tokens} tokens at $1/$1 per 1M = {tokens} micro-dollars"
        );
        let _ = engine
            .blocking()
            .stop("poldlr", Some(Duration::from_secs(5)));
    }
}
