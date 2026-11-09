//! The Policy money tests (story 21-3): the change job's breach-at-once
//! and re-arm halves, per limit dimension and scope, and the FR-21
//! upgrade's crash drill — proven at the store/job level with Runs,
//! Policies and usage seeded directly (no child process, every OS).

use std::collections::BTreeMap;
use std::collections::HashMap;

use super::registry_policy::InstanceContext;
use super::*;
use crate::domain::runs::ResolveInputs;
use crate::domain::BudgetBreachEvent;
use crate::metering::WireFence;
use crate::ports::RunStore as _;

/// One seeded fleet: an instance (id 1), its open process-owning Run with
/// a bound `team=core` label, and committed usage priced at
/// `$1.00/$1.00` per 1M (in=input, out=output tokens).
struct Seeded {
    #[allow(dead_code)]
    state: tempfile::TempDir,
    registry: Registry,
    run: RunId,
}

fn seed_at(input: u64, output: u64) -> Seeded {
    let state = tempfile::TempDir::new().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let run = RunId::mint();
    let record = RunRecord {
        run_id: run.clone(),
        control_depth: ControlDepth::Supervised,
        wire_path: true,
        status: crate::domain::runs::RunStatus::Active,
        anomalies: Vec::new(),
        parent_run_id: None,
        instance: Some("money".to_string()),
        process_owner: true,
        end_cause: None,
        created_at: "t".to_string(),
        updated_at: "t".to_string(),
        ended_at: None,
    };
    let run_for_job = run.clone();
    registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            store.execute_batch_for_test(
                "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, \
                 updated_at) VALUES (1, 'money', 'mock', 'running', '/h', 't', 't')",
            );
            use crate::ports::{NewRun, RunRecords as _, RunStore as _};
            store.insert_run(&NewRun {
                record,
                labels: vec![crate::domain::runs::Label::bound("team", "core")],
                limits: Default::default(),
                identity: None,
            })?;
            store.insert_usage(
                &crate::domain::usage::UsageEvent {
                    instance: "money".to_string(),
                    run_id: run_for_job,
                    input_tokens: input,
                    output_tokens: output,
                    cached_tokens: Some(0),
                    metering_source: "self-reported".to_string(),
                    sequence: 1,
                    occurred_at: "t".to_string(),
                },
                Some(Rate::new(1_000_000, 1_000_000)),
                1,
            )?;
            Ok(())
        })
        .unwrap();
    Seeded {
        state,
        registry,
        run,
    }
}

/// Seed a fleet whose committed usage totals `tokens` (half in, half out).
fn seed(tokens: u64) -> Seeded {
    seed_at(tokens / 2, tokens / 2)
}

/// The caller-side context the facade builds (fresh empty explicit inputs:
/// nothing on the instance, everything from the Policy under test).
fn contexts() -> HashMap<String, InstanceContext> {
    HashMap::from([(
        "money".to_string(),
        InstanceContext {
            fence: None,
            metering_source: "self-reported".to_string(),
            inputs: Some(Ok(ResolveInputs::default())),
        },
    )])
}

fn change(registry: &Registry, name: &str, selector: &str, set: PolicyLimits) -> PolicyOutcome {
    let existing = registry.policy(name).unwrap();
    registry
        .apply_policy_change(
            existing,
            PolicyChange {
                name: name.to_string(),
                selector: Some(selector.parse().unwrap()),
                set,
                clear: Vec::new(),
                rates: BTreeMap::new(),
            },
            contexts(),
        )
        .unwrap()
        .outcome
}

/// The instance's breach events, in order.
fn breaches(registry: &Registry, run: &RunId) -> Vec<BudgetBreachEvent> {
    let name = InstanceName::new("money").unwrap();
    registry.reader().run_breaches(&name, run).unwrap()
}

#[test]
fn lowering_a_token_ceiling_below_usage_breaches_at_once_per_run_and_cumulative() {
    for (field, scope, label) in [
        (
            LimitField::TokensPerRun,
            crate::domain::BreachScope::PerRun,
            "per-run",
        ),
        (
            LimitField::TokensCumulative,
            crate::domain::BreachScope::Cumulative,
            "cumulative",
        ),
    ] {
        let seeded = seed(1_000);
        // A Policy ABOVE the usage first: no breach on create.
        let outcome = change(&seeded.registry, "cap", "team=core", field.set(Some(2_000)));
        assert_eq!(outcome.matched, 1, "{label}");
        assert!(
            breaches(&seeded.registry, &seeded.run).is_empty(),
            "{label}: above the usage, nothing fires"
        );
        // LOWERED below the usage: the breach fires in the change's own
        // writer batch, without any new usage event.
        let outcome = change(&seeded.registry, "cap", "team=core", field.set(Some(500)));
        assert_eq!(outcome.matched, 1);
        let events = breaches(&seeded.registry, &seeded.run);
        assert_eq!(events.len(), 1, "{label}: {events:?}");
        assert_eq!(events[0].scope, scope, "{label}");
        assert_eq!(events[0].dimension, crate::domain::BreachDimension::Tokens);
        assert_eq!(events[0].limit, 500);
        // The Run carries the `breach` Anomaly (FR-21's loud signal).
        let view = seeded.registry.run_view(&seeded.run).unwrap();
        assert!(
            view.record
                .anomalies
                .contains(&crate::domain::runs::Anomaly::Breach),
            "{label}: {:?}",
            view.record.anomalies
        );
        // The latch holds: re-applying the SAME policy fires nothing more.
        change(&seeded.registry, "cap", "team=core", field.set(Some(500)));
        assert_eq!(breaches(&seeded.registry, &seeded.run).len(), 1, "{label}");
    }
}

#[test]
fn lowering_a_dollar_ceiling_below_usage_breaches_at_once_per_run_and_cumulative() {
    for (field, scope, label) in [
        (
            LimitField::DollarsPerRun,
            crate::domain::BreachScope::PerRun,
            "per-run",
        ),
        (
            LimitField::DollarsCumulative,
            crate::domain::BreachScope::Cumulative,
            "cumulative",
        ),
    ] {
        let seeded = seed(4_000_000); // 4M tokens total = $4.00 at $1/$1 per 1M.
        let mut above = field.set(Some(10_000_000));
        above.rate = Some(Rate::new(1_000_000, 1_000_000));
        let outcome = change(&seeded.registry, "cap", "team=core", above);
        assert_eq!(outcome.matched, 1, "{label}");
        assert!(
            breaches(&seeded.registry, &seeded.run).is_empty(),
            "{label}"
        );
        let mut lowered = field.set(Some(3_000_000));
        lowered.rate = Some(Rate::new(1_000_000, 1_000_000));
        change(&seeded.registry, "cap", "team=core", lowered);
        let events = breaches(&seeded.registry, &seeded.run);
        assert_eq!(events.len(), 1, "{label}: {events:?}");
        assert_eq!(events[0].scope, scope, "{label}");
        assert_eq!(events[0].dimension, crate::domain::BreachDimension::Dollars);
        assert_eq!(events[0].limit, 3_000_000, "{label}");
    }
}

#[test]
fn raising_a_ceiling_above_usage_re_arms_the_latch_and_the_next_crossing_fires_again() {
    let seeded = seed(1_000);
    change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            tokens_per_run: Some(500),
            ..PolicyLimits::default()
        },
    );
    assert_eq!(breaches(&seeded.registry, &seeded.run).len(), 1);
    // RAISED above the usage: the latch clears, the `breach` Anomaly
    // clears, and the Run may spend up to the new ceiling again.
    change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            tokens_per_run: Some(1_500),
            ..PolicyLimits::default()
        },
    );
    assert!(
        breaches(&seeded.registry, &seeded.run)
            .iter()
            .all(|event| event.limit != 1_500),
        "raising fires nothing"
    );
    let view = seeded.registry.run_view(&seeded.run).unwrap();
    assert!(
        !view
            .record
            .anomalies
            .contains(&crate::domain::runs::Anomaly::Breach),
        "the breach Anomaly cleared: {:?}",
        view.record.anomalies
    );
    // The next crossing re-fires under the raised ceiling.
    change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            tokens_per_run: Some(900),
            ..PolicyLimits::default()
        },
    );
    let events = breaches(&seeded.registry, &seeded.run);
    assert_eq!(
        events.len(),
        2,
        "the re-armed latch fires again: {events:?}"
    );
    assert_eq!(events[1].limit, 900);
}

#[test]
fn the_fence_clears_per_reason_only() {
    // Two latched `block` reasons (tokens per-run, dollars cumulative):
    // raising ONLY the token ceiling keeps the fence up on the dollar
    // reason; raising both unfences (amendment 2).
    let seeded = seed_at(500_000, 500_000); // 1M tokens = $1.00 at $1/$1 per 1M.
    let fence = WireFence::default();
    let mut contexts = contexts();
    contexts.insert(
        "money".to_string(),
        InstanceContext {
            fence: Some(fence.clone()),
            metering_source: "engine-observed".to_string(),
            inputs: Some(Ok(ResolveInputs::default())),
        },
    );
    let both = PolicyLimits {
        tokens_per_run: Some(500),
        dollars_cumulative_micros: Some(1_000_000), // $1.00: at-or-below the $1.00 usage.
        breach_action: Some(BreachAction::Block),
        rate: Some(Rate::new(1_000_000, 1_000_000)),
        ..PolicyLimits::default()
    };
    let apply = |set, contexts: &HashMap<String, InstanceContext>| {
        seeded
            .registry
            .apply_policy_change(
                seeded.registry.policy("fence").unwrap(),
                PolicyChange {
                    name: "fence".to_string(),
                    selector: Some("team=core".parse().unwrap()),
                    set,
                    clear: Vec::new(),
                    rates: BTreeMap::new(),
                },
                contexts.clone(),
            )
            .unwrap()
    };
    apply(both.clone(), &contexts);
    // Both reasons latched under `block` (tokens per-run and dollars
    // cumulative), and the canonical order names tokens first.
    {
        use crate::ports::RunStore as _;
        let mut latched: Vec<String> = seeded
            .registry
            .reader()
            .latches(&seeded.run)
            .unwrap()
            .into_iter()
            .map(|(key, action)| format!("{} {}", key.dimension.as_str(), action.as_str()))
            .collect();
        latched.sort();
        assert_eq!(
            latched,
            vec!["dollars block".to_string(), "tokens block".to_string()],
            "both `block` reasons latched"
        );
    }
    assert!(fence.refusal().is_some(), "both reasons latched: fenced");
    assert_eq!(
        fence.refusal().unwrap().code,
        "tokens_per_run",
        "the canonical first reason"
    );

    // Raise ONLY the token ceiling above the usage (1M tokens): the token
    // reason clears, the dollar reason holds, the fence STAYS UP.
    apply(
        PolicyLimits {
            tokens_per_run: Some(2_000_000),
            ..both.clone()
        },
        &contexts,
    );
    assert!(
        fence.refusal().is_some(),
        "the dollar reason still holds the fence"
    );
    assert_eq!(
        fence.refusal().unwrap().code,
        "dollars_cumulative",
        "the remaining reason names itself"
    );

    // Raise both: the fence comes down.
    apply(
        PolicyLimits {
            tokens_per_run: Some(2_000_000),
            dollars_cumulative_micros: Some(10_000_000),
            breach_action: Some(BreachAction::Block),
            rate: Some(Rate::new(1_000_000, 1_000_000)),
            ..PolicyLimits::default()
        },
        &contexts,
    );
    assert!(fence.refusal().is_none(), "every reason re-armed: unfenced");
}

#[test]
fn a_child_run_at_a_depth_without_the_action_counts_as_a_fallback() {
    let seeded = seed(1_000);
    let child = RunId::mint();
    let child_record = RunRecord {
        run_id: child.clone(),
        control_depth: ControlDepth::Announced,
        wire_path: true,
        status: crate::domain::runs::RunStatus::Active,
        anomalies: Vec::new(),
        parent_run_id: Some(seeded.run.clone()),
        instance: Some("money".to_string()),
        process_owner: false,
        end_cause: None,
        created_at: "t".to_string(),
        updated_at: "t".to_string(),
        ended_at: None,
    };
    seeded
        .registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            use crate::ports::{NewRun, RunRecords as _};
            store.insert_run(&NewRun {
                record: child_record,
                labels: vec![crate::domain::runs::Label::bound("team", "core")],
                limits: Default::default(),
                identity: None,
            })?;
            Ok(())
        })
        .unwrap();
    // `pause` from a Policy: the child's depth cannot execute it → the
    // child counts in the fallbacks; the owner does not.
    let outcome = change(
        &seeded.registry,
        "kid",
        "team=core",
        PolicyLimits {
            breach_action: Some(BreachAction::Pause),
            ..PolicyLimits::default()
        },
    );
    assert_eq!(outcome.matched, 2);
    assert_eq!(outcome.fallbacks, 1, "{:?}", outcome.notes);
    let view = seeded.registry.run_view(&child).unwrap();
    assert!(
        view.record
            .anomalies
            .contains(&crate::domain::runs::Anomaly::DepthFallback),
        "{:?}",
        view.record.anomalies
    );
}

#[test]
fn an_unreadable_instance_config_skips_the_runs_money_half_fail_closed() {
    let seeded = seed(1_000);
    let mut contexts = contexts();
    contexts.insert(
        "money".to_string(),
        InstanceContext {
            fence: None,
            metering_source: "self-reported".to_string(),
            inputs: Some(Err("the configuration could not be read".to_string())),
        },
    );
    let applied = seeded
        .registry
        .apply_policy_change(
            None,
            PolicyChange {
                name: "cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(500),
                    ..PolicyLimits::default()
                },
                clear: Vec::new(),
                rates: BTreeMap::new(),
            },
            contexts,
        )
        .unwrap();
    // The change commits (the row, the event), the Run is SKIPPED with a
    // note, and NO breach fires on a limit that might belong to the
    // instance explicitly (money code: fail closed).
    assert_eq!(applied.outcome.matched, 1);
    assert_eq!(applied.outcome.fallbacks, 0);
    assert!(
        applied
            .outcome
            .notes
            .iter()
            .any(|note| note.contains("was not re-resolved")),
        "{:?}",
        applied.outcome.notes
    );
    assert!(breaches(&seeded.registry, &seeded.run).is_empty());
}

#[test]
fn a_caller_label_never_selects_the_policy_for_the_change_job() {
    let seeded = seed(1_000);
    let run = seeded.run.clone();
    seeded
        .registry
        .writer
        .call(move |ctx| {
            use crate::ports::RunRecords;
            ctx.store()
                .set_run_labels(&run, &[crate::domain::runs::Label::caller("team", "core")])?;
            Ok(())
        })
        .unwrap();
    let outcome = change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            tokens_per_run: Some(500),
            ..PolicyLimits::default()
        },
    );
    assert_eq!(outcome.matched, 0, "bound-only matching");
    assert!(breaches(&seeded.registry, &seeded.run).is_empty());
}

#[test]
fn deleting_the_policy_re_resolves_its_runs_under_what_remains() {
    let seeded = seed(1_000);
    change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            tokens_per_run: Some(500),
            ..PolicyLimits::default()
        },
    );
    assert_eq!(breaches(&seeded.registry, &seeded.run).len(), 1);
    // Delete: the Run resolves under the remaining set (no ceiling); the
    // dry run first reports what would loosen.
    let existing = seeded.registry.policy("cap").unwrap().unwrap();
    let dry = seeded
        .registry
        .plan_policy_change(
            Some(&existing),
            &PolicyChange {
                name: "cap".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(dry.matched, 1);
    let applied = seeded
        .registry
        .apply_policy_delete(existing, contexts())
        .unwrap();
    assert_eq!(applied.outcome.matched, 1);
    assert!(seeded.registry.policy("cap").unwrap().is_none());
    // The stored resolution no longer carries the ceiling.
    let view = seeded.registry.run_view(&seeded.run).unwrap();
    assert!(view.policy.as_ref().unwrap().tokens.per_run.is_none());
}

#[test]
fn the_rate_chain_prices_a_policy_dollar_cap_and_the_unpriced_cap_is_loud() {
    // A Policy dollar cap with a flat Rate: the cap enforces in dollars
    // without any instance rate (the chain: Policy rate → the cap).
    let seeded = seed(2_000_000); // 2M tokens total = $2.00.
    let outcome = change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            dollars_per_run_micros: Some(1_500_000), // $1.50 < $2.00 usage.
            rate: Some(Rate::new(1_000_000, 1_000_000)),
            ..PolicyLimits::default()
        },
    );
    assert_eq!(outcome.matched, 1);
    let events = breaches(&seeded.registry, &seeded.run);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].dimension, crate::domain::BreachDimension::Dollars);

    // The unpriced twin: the same cap with NO rate anywhere raises the
    // `unpriced-model` Anomaly and never fires.
    let seeded = seed(2_000_000);
    change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            dollars_per_run_micros: Some(1_500_000),
            ..PolicyLimits::default()
        },
    );
    assert!(breaches(&seeded.registry, &seeded.run).is_empty());
    let view = seeded.registry.run_view(&seeded.run).unwrap();
    let resolution = view.policy.as_ref().unwrap();
    assert!(resolution.rate.is_none());
    let unpriced = resolution.unpriced.as_ref().unwrap();
    assert_eq!(unpriced.policy, "cap");
    assert!(
        view.record
            .anomalies
            .contains(&crate::domain::runs::Anomaly::UnpricedModel),
        "{:?}",
        view.record.anomalies
    );
    // Adding the Rate through a policy change clears it.
    change(
        &seeded.registry,
        "cap",
        "team=core",
        PolicyLimits {
            dollars_per_run_micros: Some(1_500_000),
            rate: Some(Rate::new(1_000_000, 1_000_000)),
            ..PolicyLimits::default()
        },
    );
    let view = seeded.registry.run_view(&seeded.run).unwrap();
    assert!(
        !view
            .record
            .anomalies
            .contains(&crate::domain::runs::Anomaly::UnpricedModel),
        "the Anomaly cleared when a Rate resolved"
    );
}

// ---------------------------------------------------------------------------
// The FR-21 upgrade crash drill (AD-7's order).
// ---------------------------------------------------------------------------

/// A state dir whose `kept` instance sits mid-upgrade: the pending row
/// exists (a crash after the v15 transaction), the config does not.
fn mid_upgrade_state() -> (tempfile::TempDir, std::path::PathBuf) {
    let state = tempfile::TempDir::new().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let home = state.path().join("agents").join("kept");
    std::fs::create_dir_all(&home).unwrap();
    registry
        .writer
        .call(move |ctx| {
            ctx.store().execute_batch_for_test(
                "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, \
                 updated_at) VALUES (1, 'kept', 'mock', 'registered', '/h', 't', 't'); \
                 INSERT INTO breach_default_upgrade (instance_id, listed_at) VALUES (1, 't')",
            );
            Ok(())
        })
        .unwrap();
    (state, home)
}

#[test]
fn a_pending_upgrade_row_holds_pause_across_the_window_and_finishes_once() {
    let (state, home) = mid_upgrade_state();
    let mut registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    // This open's finish already wrote the config and emitted the event
    // (the resume). The config carries pause explicitly.
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap_or_default();
    assert!(
        config.contains("breach_action = \"pause\""),
        "the upgrade wrote pause explicitly: {config}"
    );
    let events = registry.take_open_events();
    let upgraded: Vec<&BreachDefaultUpgradedEvent> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::BreachDefaultUpgraded(event) => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(upgraded.len(), 1, "{events:?}");
    assert_eq!(upgraded[0].instances.len(), 1);
    assert_eq!(upgraded[0].instances[0].name, "kept");
    assert!(upgraded[0].instances[0].config_written);
    // The resolution reads the explicit key now (still pause — the promise
    // holds, through the key rather than the marker).
    let kept = InstanceName::new("kept").unwrap();
    assert_eq!(
        registry.resolved_limits(&kept, None).action,
        BreachAction::Pause
    );

    // A reopen emits nothing more (idempotent; the list is done).
    drop(registry);
    let mut registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    assert!(
        !registry
            .take_open_events()
            .iter()
            .any(|event| matches!(event, EngineEvent::BreachDefaultUpgraded(_))),
        "exactly one upgrade event across all opens"
    );
    // An explicit action write clears the marker (the note ends).
    registry
        .set_config(
            &InstanceName::new("kept").unwrap(),
            "budget.breach_action",
            "warn",
        )
        .unwrap();
    assert_eq!(
        registry
            .resolved_limits(&InstanceName::new("kept").unwrap(), None)
            .action,
        BreachAction::Warn
    );
}

#[test]
fn the_pending_row_alone_fail_closes_resolution_to_pause() {
    // The crash window itself: a pending row and NO finished upgrade (the
    // open's finish is what writes; simulate by inserting the row into an
    // ALREADY-OPEN registry, which never re-runs the finish).
    let (state, _home) = mid_upgrade_state();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    // Remove the finish's effect as if it had crashed before Transaction B:
    // clear the config file and reset the row to pending.
    let home = state.path().join("agents").join("kept");
    std::fs::remove_file(home.join("config.toml")).unwrap();
    registry
        .writer
        .call(|ctx| {
            ctx.store().execute_batch_for_test(
                "UPDATE breach_default_upgrade SET config_written = 0, done_at = NULL",
            );
            Ok(())
        })
        .unwrap();
    let name = InstanceName::new("kept").unwrap();
    assert_eq!(
        registry.resolved_limits(&name, None).action,
        BreachAction::Pause,
        "the pending row alone holds pause across the crash window"
    );
}

#[test]
fn an_unwritable_config_keeps_pause_through_the_marker() {
    let state = tempfile::TempDir::new().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    // A config that CANNOT be written (a directory where the file goes).
    let home = state.path().join("agents").join("stranded");
    std::fs::create_dir_all(home.join("config.toml")).unwrap();
    registry
        .writer
        .call(|ctx| {
            ctx.store().execute_batch_for_test(
                "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, \
                 updated_at) VALUES (1, 'stranded', 'mock', 'registered', '/h', 't', 't'); \
                 INSERT INTO breach_default_upgrade (instance_id, listed_at) VALUES (1, 't')",
            );
            Ok(())
        })
        .unwrap();
    drop(registry);
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let stranded = InstanceName::new("stranded").unwrap();
    assert_eq!(
        registry.resolved_limits(&stranded, None).action,
        BreachAction::Pause,
        "the marker keeps pause when the config cannot be written"
    );
    let notes = registry.take_open_notes();
    assert!(
        notes
            .iter()
            .any(|note| note.contains("stays on pause through its upgrade marker")),
        "{notes:?}"
    );
    let mut registry = registry;
    let events = registry.take_open_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::BreachDefaultUpgraded(event)
                if !event.instances.is_empty() && !event.instances[0].config_written
        )),
        "the event names the unwritten instance: {events:?}"
    );
}

/// The per-limit field under test.
enum LimitField {
    TokensPerRun,
    TokensCumulative,
    DollarsPerRun,
    DollarsCumulative,
}

impl LimitField {
    fn set(&self, value: Option<u64>) -> PolicyLimits {
        match self {
            LimitField::TokensPerRun => PolicyLimits {
                tokens_per_run: value,
                ..PolicyLimits::default()
            },
            LimitField::TokensCumulative => PolicyLimits {
                tokens_cumulative: value,
                ..PolicyLimits::default()
            },
            LimitField::DollarsPerRun => PolicyLimits {
                dollars_per_run_micros: value,
                ..PolicyLimits::default()
            },
            LimitField::DollarsCumulative => PolicyLimits {
                dollars_cumulative_micros: value,
                ..PolicyLimits::default()
            },
        }
    }
}
