//! The Run-limit money tests (story 21-6): the budget job and the
//! config-write re-arm, proven at the store/job level with Runs, usage,
//! latches and Policies seeded directly (no child process, every OS) —
//! tokens AND dollars, per-run AND cumulative, breach from a lowering,
//! re-arm on a raise, the per-reason fence clear, the actor matrix, the
//! scope refusals, and the Run-scoped rows.

use std::collections::BTreeMap;

use super::registry_policy::InstanceContext;
use super::*;
use crate::domain::governance::LatchKey;
use crate::domain::runs::{
    Anomaly, LimitActor, LimitSource, LimitsDirection, ResolveInputs, RunEventKind, RunLimitChange,
    RunLimitField, RunLimits, RunStatus,
};
use crate::domain::{BreachDimension, BudgetBreachEvent};
use crate::metering::WireFence;
use crate::ports::{RunRecords as _, RunStore as _};

const INSTANCE: &str = "money";

struct Seeded {
    #[allow(dead_code)]
    state: tempfile::TempDir,
    registry: Registry,
    run: RunId,
}

fn record(run: &RunId, process_owner: bool, wire_path: bool, parent: Option<&RunId>) -> RunRecord {
    RunRecord {
        run_id: run.clone(),
        control_depth: if process_owner {
            ControlDepth::Supervised
        } else {
            ControlDepth::Announced
        },
        wire_path,
        status: RunStatus::Active,
        anomalies: Vec::new(),
        parent_run_id: parent.cloned(),
        instance: Some(INSTANCE.to_string()),
        process_owner,
        end_cause: None,
        created_at: "t".to_string(),
        updated_at: "t".to_string(),
        ended_at: None,
    }
}

/// One instance, its open process-owning Run (bound `team=core`), and
/// committed usage priced at $1.00/$1.00 per 1M (`input` + `output`).
fn seed_with(input: u64, output: u64, wire_path: bool) -> Seeded {
    let state = tempfile::TempDir::new().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let run = RunId::mint();
    let owner = record(&run, true, wire_path, None);
    let usage_run = run.clone();
    registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            store.execute_batch_for_test(
                "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, \
                 updated_at) VALUES (1, 'money', 'mock', 'running', '/h', 't', 't')",
            );
            store.insert_run(&crate::ports::NewRun {
                record: owner,
                labels: vec![crate::domain::runs::Label::bound("team", "core")],
                limits: Default::default(),
                identity: None,
            })?;
            store.insert_usage(
                &crate::domain::usage::UsageEvent {
                    instance: INSTANCE.to_string(),
                    run_id: usage_run,
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

/// 1M tokens = $1.00 at $1/$1 per 1M.
fn seed() -> Seeded {
    seed_with(500_000, 500_000, true)
}

/// A child Run under the owner (no usage of its own).
fn add_child(seeded: &Seeded, wire_path: bool) -> RunId {
    let child = RunId::mint();
    let row = record(&child, false, wire_path, Some(&seeded.run));
    seeded
        .registry
        .writer
        .call(move |ctx| {
            ctx.store().insert_run(&crate::ports::NewRun {
                record: row,
                labels: Vec::new(),
                limits: Default::default(),
                identity: None,
            })
        })
        .unwrap();
    child
}

/// The instance's priced inputs (the Rate the dollar arms need; explicit
/// ceilings as given).
fn inputs(explicit: PolicyLimits) -> ResolveInputs {
    ResolveInputs {
        explicit: PolicyLimits {
            rate: Some(Rate::new(1_000_000, 1_000_000)),
            ..explicit
        },
        ..ResolveInputs::default()
    }
}

fn context(fence: Option<WireFence>, explicit: PolicyLimits) -> InstanceContext {
    InstanceContext {
        metering_source: if fence.is_some() {
            "engine-observed".to_string()
        } else {
            "self-reported".to_string()
        },
        fence,
        supervised: true,
        inputs: Some(Ok(inputs(explicit))),
    }
}

fn set(
    registry: &Registry,
    run: &RunId,
    change: RunLimitChange,
    actor: LimitActor,
    context: InstanceContext,
) -> Result<crate::domain::RunLimitsOutcome, crate::domain::RunError> {
    registry
        .apply_run_limits(run, &change, actor, Some(context))
        .map(|applied| applied.outcome.expect("one Run's outcome"))
}

fn limits(set: RunLimits) -> RunLimitChange {
    RunLimitChange {
        set,
        clear: Vec::new(),
    }
}

fn breaches(registry: &Registry, run: &RunId) -> Vec<BudgetBreachEvent> {
    let name = InstanceName::new(INSTANCE).unwrap();
    registry.reader().run_breaches(&name, run).unwrap()
}

fn latched(registry: &Registry, run: &RunId) -> Vec<(LatchKey, BreachAction)> {
    registry.reader().latches(run).unwrap()
}

fn anomalies(registry: &Registry, run: &RunId) -> Vec<Anomaly> {
    registry.run_view(run).unwrap().record.anomalies
}

/// The four (dimension, scope) cells: a setter for the Run's own limits,
/// the latch key, and the ceiling BELOW and ABOVE the seeded usage (1M
/// tokens; $1.00 of cost; the instance's lifetime = the one Run here).
/// One cell: its label, the Run-limit setter, the latch key, and the
/// ceilings below and above the usage.
type Cell = (&'static str, fn(u64) -> RunLimits, LatchKey, u64, u64);

fn cells() -> Vec<Cell> {
    use crate::domain::BreachScope;
    vec![
        (
            "tokens per-run",
            |v| RunLimits {
                token_per_run: Some(v),
                ..RunLimits::default()
            },
            LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
            500_000,
            2_000_000,
        ),
        (
            "tokens cumulative",
            |v| RunLimits {
                token_cumulative: Some(v),
                ..RunLimits::default()
            },
            LatchKey::new(BreachDimension::Tokens, BreachScope::Cumulative),
            500_000,
            2_000_000,
        ),
        (
            "dollars per-run",
            |v| RunLimits {
                dollar_per_run_micros: Some(Micros(v as i64)),
                ..RunLimits::default()
            },
            LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun),
            500_000,
            2_000_000,
        ),
        (
            "dollars cumulative",
            |v| RunLimits {
                dollar_cumulative_micros: Some(Micros(v as i64)),
                ..RunLimits::default()
            },
            LatchKey::new(BreachDimension::Dollars, BreachScope::Cumulative),
            500_000,
            2_000_000,
        ),
    ]
}

#[test]
fn lowering_each_ceiling_to_at_or_below_the_usage_breaches_at_once_then_raising_re_arms() {
    for (label, limit, key, below, above) in cells() {
        let seeded = seed();
        // Lowered below the usage: the breach fires in the job itself — no
        // usage event needed — with the breach row, the latch and the
        // `breach` Anomaly.
        let outcome = set(
            &seeded.registry,
            &seeded.run,
            limits(limit(below)),
            LimitActor::Operator,
            context(None, PolicyLimits::default()),
        )
        .unwrap();
        assert_eq!(outcome.direction, LimitsDirection::Lowered, "{label}");
        assert!(outcome.run_scoped, "{label}");
        assert_eq!(outcome.fired.len(), 1, "{label}: {:?}", outcome.fired);
        assert_eq!(
            LatchKey::new(outcome.fired[0].dimension, outcome.fired[0].scope),
            key,
            "{label}"
        );
        assert_eq!(outcome.fired[0].limit, below, "{label}");
        let events = breaches(&seeded.registry, &seeded.run);
        assert_eq!(events.len(), 1, "{label}: {events:?}");
        assert_eq!(events[0].limit, below, "{label}");
        assert_eq!(
            latched(&seeded.registry, &seeded.run),
            vec![(key, BreachAction::Warn)],
            "{label}"
        );
        assert!(anomalies(&seeded.registry, &seeded.run).contains(&Anomaly::Breach));
        // The resolved ceiling is the Run's own (source `run`).
        let source = match label {
            "tokens per-run" => &outcome.resolution.sources.tokens_per_run,
            "tokens cumulative" => &outcome.resolution.sources.tokens_cumulative,
            "dollars per-run" => &outcome.resolution.sources.dollars_per_run,
            _ => &outcome.resolution.sources.dollars_cumulative,
        };
        assert_eq!(source, &LimitSource::Run, "{label}");

        // Raised above the usage: the latch re-arms, the Anomaly clears,
        // nothing fires.
        let outcome = set(
            &seeded.registry,
            &seeded.run,
            limits(limit(above)),
            LimitActor::Operator,
            context(None, PolicyLimits::default()),
        )
        .unwrap();
        assert_eq!(outcome.direction, LimitsDirection::Raised, "{label}");
        assert_eq!(outcome.re_armed, vec![key], "{label}");
        assert!(outcome.fired.is_empty(), "{label}");
        assert!(latched(&seeded.registry, &seeded.run).is_empty(), "{label}");
        assert!(!anomalies(&seeded.registry, &seeded.run).contains(&Anomaly::Breach));

        // The re-armed latch fires again at the next lowering.
        let outcome = set(
            &seeded.registry,
            &seeded.run,
            limits(limit(below)),
            LimitActor::Operator,
            context(None, PolicyLimits::default()),
        )
        .unwrap();
        assert_eq!(
            outcome.fired.len(),
            1,
            "{label}: the re-armed latch fires again"
        );
        assert_eq!(breaches(&seeded.registry, &seeded.run).len(), 2, "{label}");
    }
}

#[test]
fn a_raise_lifts_the_matching_fence_reason_per_reason_and_the_other_holds() {
    let seeded = seed();
    let fence = WireFence::default();
    let ctx = || context(Some(fence.clone()), PolicyLimits::default());
    // Two `block` reasons: tokens per-run and dollars cumulative, both at
    // or below the usage.
    let both = RunLimits {
        token_per_run: Some(500_000),
        dollar_cumulative_micros: Some(Micros(1_000_000)),
        breach_action: Some(BreachAction::Block),
        ..RunLimits::default()
    };
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(both),
        LimitActor::Operator,
        ctx(),
    )
    .unwrap();
    assert_eq!(outcome.fired.len(), 2, "{:?}", outcome.fired);
    assert_eq!(fence.refusal().unwrap().code, "tokens_per_run");

    // Raise ONLY the token ceiling: its reason lifts, the dollar reason
    // holds the fence, the `breach` Anomaly stays.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(5_000_000),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        ctx(),
    )
    .unwrap();
    assert_eq!(outcome.direction, LimitsDirection::Raised);
    let tokens = LatchKey::new(BreachDimension::Tokens, crate::domain::BreachScope::PerRun);
    let dollars = LatchKey::new(
        BreachDimension::Dollars,
        crate::domain::BreachScope::Cumulative,
    );
    assert_eq!(outcome.re_armed, vec![tokens]);
    assert_eq!(outcome.fence_cleared, vec![tokens]);
    assert_eq!(
        fence.refusal().unwrap().code,
        "dollars_cumulative",
        "the other reason keeps the fence up"
    );
    assert_eq!(
        latched(&seeded.registry, &seeded.run),
        vec![(dollars, BreachAction::Block)]
    );
    assert!(anomalies(&seeded.registry, &seeded.run).contains(&Anomaly::Breach));

    // Raise the dollar ceiling too: unfenced, the Anomaly clears.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            dollar_cumulative_micros: Some(Micros(9_000_000)),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        ctx(),
    )
    .unwrap();
    assert_eq!(outcome.fence_cleared, vec![dollars]);
    assert!(fence.refusal().is_none());
    assert!(!anomalies(&seeded.registry, &seeded.run).contains(&Anomaly::Breach));

    // One `limits-changed` event per committed change, carrying the Run's
    // limits before and after and the actor as its cause.
    let batch = crate::domain::export_core(&seeded.registry, None, 0).unwrap();
    let changed: Vec<_> = batch
        .events
        .iter()
        .filter_map(|entry| match &entry.event {
            crate::domain::EngineEvent::Run(event)
                if event.change == RunEventKind::LimitsChanged =>
            {
                Some(event.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(changed.len(), 3, "{changed:?}");
    assert_eq!(
        changed[0].prior_limits.as_deref(),
        Some(&RunLimits::default())
    );
    assert_eq!(changed[0].limits.as_deref(), Some(&both));
    assert_eq!(changed[0].cause.as_deref(), Some("operator"));
    assert_eq!(
        changed[2].limits.as_ref().unwrap().dollar_cumulative_micros,
        Some(Micros(9_000_000))
    );
}

#[test]
fn a_run_limit_outranks_a_policy_both_ways_and_a_clear_hands_back_to_it() {
    let seeded = seed();
    // A Policy ceiling below the usage fires (the Policy trigger).
    seeded
        .registry
        .apply_policy_change(
            None,
            PolicyChange {
                name: "cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(800_000),
                    ..PolicyLimits::default()
                },
                clear: Vec::new(),
                rates: BTreeMap::new(),
            },
            std::collections::HashMap::from([(
                INSTANCE.to_string(),
                context(None, PolicyLimits::default()),
            )]),
        )
        .unwrap();
    assert_eq!(breaches(&seeded.registry, &seeded.run).len(), 1);
    // A LOOSER Run-level limit wins (FR-60: source `run`) and re-arms.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(3_000_000),
            ..RunLimits::default()
        }),
        LimitActor::Host,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(outcome.resolution.tokens.per_run, Some(3_000_000));
    assert_eq!(outcome.resolution.sources.tokens_per_run, LimitSource::Run);
    assert_eq!(outcome.direction, LimitsDirection::Raised);
    assert!(latched(&seeded.registry, &seeded.run).is_empty());
    // Clearing it hands the limit back to the Policy — a LOWER effective
    // ceiling here, at or below the usage — and the same pass fires.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        RunLimitChange {
            set: RunLimits::default(),
            clear: vec![RunLimitField::TokensPerRun],
        },
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(outcome.direction, LimitsDirection::Cleared);
    assert_eq!(
        outcome.resolution.sources.tokens_per_run,
        LimitSource::Policy("cap".to_string())
    );
    assert_eq!(
        outcome.fired.len(),
        1,
        "a clear can lower the effective ceiling"
    );
    assert_eq!(breaches(&seeded.registry, &seeded.run).len(), 2);
    // A TIGHTER Run-level limit than the Policy also wins.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(100),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(outcome.resolution.tokens.per_run, Some(100));
    assert_eq!(outcome.resolution.sources.tokens_per_run, LimitSource::Run);
    // An unchanged change writes and emits nothing.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(100),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(outcome.direction, LimitsDirection::Unchanged);
}

#[test]
fn a_caller_may_only_tighten_and_an_operator_or_host_may_raise() {
    let seeded = seed_with(50, 50, true);
    let start = RunLimits {
        token_per_run: Some(1_000),
        breach_action: Some(BreachAction::Pause),
        ..RunLimits::default()
    };
    set(
        &seeded.registry,
        &seeded.run,
        limits(start),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    let caller = |change: RunLimitChange| {
        set(
            &seeded.registry,
            &seeded.run,
            change,
            LimitActor::Caller,
            context(None, PolicyLimits::default()),
        )
    };
    // Raising a ceiling, clearing one (unlimited), loosening the action: refused.
    for (change, limit) in [
        (
            limits(RunLimits {
                token_per_run: Some(2_000),
                ..RunLimits::default()
            }),
            "tokens-per-run",
        ),
        (
            RunLimitChange {
                set: RunLimits::default(),
                clear: vec![RunLimitField::TokensPerRun],
            },
            "tokens-per-run",
        ),
        (
            limits(RunLimits {
                breach_action: Some(BreachAction::Warn),
                ..RunLimits::default()
            }),
            "breach-action",
        ),
    ] {
        let err = caller(change).unwrap_err();
        assert_eq!(err.code(), "raise-forbidden", "{err}");
        assert!(err.to_string().contains(limit), "{err}");
    }
    // Nothing was written by the refusals.
    assert_eq!(
        seeded.registry.reader().run_limits(&seeded.run).unwrap(),
        start
    );
    // Tightening is allowed for a Caller.
    let outcome = caller(limits(RunLimits {
        token_per_run: Some(500),
        breach_action: Some(BreachAction::Stop),
        ..RunLimits::default()
    }))
    .unwrap();
    assert_eq!(outcome.direction, LimitsDirection::Lowered);
    // An Operator and a Host may raise.
    for (actor, ceiling) in [(LimitActor::Operator, 9_000), (LimitActor::Host, 10_000)] {
        let outcome = set(
            &seeded.registry,
            &seeded.run,
            limits(RunLimits {
                token_per_run: Some(ceiling),
                ..RunLimits::default()
            }),
            actor,
            context(None, PolicyLimits::default()),
        )
        .unwrap();
        assert_eq!(outcome.direction, LimitsDirection::Raised, "{actor:?}");
    }
    // The rows carry the actor as their source.
    let source: String = seeded.registry.reader().query_string_for_test(&format!(
        "SELECT source FROM run_limits WHERE run_id = '{}' AND dimension = 'tokens'",
        seeded.run
    ));
    assert_eq!(source, "host");
}

#[test]
fn scopes_actions_and_ended_runs_are_refused_per_run() {
    let seeded = seed_with(50, 50, false);
    let child = add_child(&seeded, false);
    // A cumulative ceiling on a child: the hosting instance's.
    for set_limit in [
        RunLimits {
            token_cumulative: Some(5),
            ..RunLimits::default()
        },
        RunLimits {
            dollar_cumulative_micros: Some(Micros(5)),
            ..RunLimits::default()
        },
    ] {
        let err = set(
            &seeded.registry,
            &child,
            limits(set_limit),
            LimitActor::Operator,
            context(None, PolicyLimits::default()),
        )
        .unwrap_err();
        assert_eq!(err.code(), "scope-unavailable", "{err}");
    }
    // `block` on a supervised Run with no wire path names the Metering
    // Source; a child's `pause` is refused until wire hold.
    let err = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            breach_action: Some(BreachAction::Block),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap_err();
    assert_eq!(err.code(), "action-unavailable");
    let text = err.to_string();
    assert!(
        text.contains("supervised (no wire)") && text.contains("self-reported"),
        "{text}"
    );
    let err = set(
        &seeded.registry,
        &child,
        limits(RunLimits {
            token_per_run: Some(5),
            breach_action: Some(BreachAction::Pause),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap_err();
    assert_eq!(err.code(), "action-unavailable");
    // A child's per-run ceiling under `warn` is fine.
    let outcome = set(
        &seeded.registry,
        &child,
        limits(RunLimits {
            token_per_run: Some(5),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(outcome.resolution.sources.tokens_per_run, LimitSource::Run);
    // An unknown Run, and an ended one.
    let err = set(
        &seeded.registry,
        &RunId::mint(),
        limits(RunLimits {
            token_per_run: Some(1),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap_err();
    assert_eq!(err.code(), "run-not-found");
    let ended = seeded.run.clone();
    seeded
        .registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            let mut row = store.run_record(&ended)?.unwrap();
            row.status = RunStatus::Ended;
            store.save_run(&row)
        })
        .unwrap();
    let err = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(1),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap_err();
    assert_eq!(err.code(), "run-ended");
    // A malformed change is refused before any Run is read.
    assert_eq!(
        RunLimitChange::default().validate().unwrap_err().code(),
        "invalid-request"
    );
    let both = RunLimitChange {
        set: RunLimits {
            token_per_run: Some(1),
            ..RunLimits::default()
        },
        clear: vec![RunLimitField::TokensPerRun],
    };
    assert!(both
        .validate()
        .unwrap_err()
        .to_string()
        .contains("both set and cleared"));
    let huge = limits(RunLimits {
        token_cumulative: Some(u64::MAX),
        ..RunLimits::default()
    });
    assert!(huge.validate().unwrap_err().to_string().contains("at most"));
    // An unreadable configuration refuses (fail closed, never "nothing set").
    let err = seeded
        .registry
        .plan_run_limits(
            &child,
            &limits(RunLimits {
                token_per_run: Some(9),
                ..RunLimits::default()
            }),
            LimitActor::Operator,
            Some(&InstanceContext {
                inputs: Some(Err("broken toml".to_string())),
                ..InstanceContext::default()
            }),
        )
        .unwrap_err();
    assert!(err.to_string().contains("broken toml"), "{err}");
}

#[test]
fn a_config_write_re_arms_and_breaches_through_the_same_pass() {
    let seeded = seed();
    let fence = WireFence::default();
    let name = InstanceName::new(INSTANCE).unwrap();
    let write = |explicit: PolicyLimits| {
        seeded
            .registry
            .rearm_after_config_write(
                &name,
                inputs(explicit.clone()),
                context(Some(fence.clone()), explicit),
            )
            .unwrap()
    };
    // An explicit `block` ceiling written at or below the usage fires now.
    let applied = write(PolicyLimits {
        tokens_per_run: Some(500_000),
        dollars_per_run_micros: Some(900_000),
        breach_action: Some(BreachAction::Block),
        ..PolicyLimits::default()
    });
    assert_eq!(applied.breaches.len(), 2, "{:?}", applied.breaches.len());
    assert_eq!(fence.refusal().unwrap().code, "tokens_per_run");
    // Raising the token ceiling above the totals lifts that reason only.
    write(PolicyLimits {
        tokens_per_run: Some(5_000_000),
        dollars_per_run_micros: Some(900_000),
        breach_action: Some(BreachAction::Block),
        ..PolicyLimits::default()
    });
    assert_eq!(fence.refusal().unwrap().code, "dollars_per_run");
    assert!(anomalies(&seeded.registry, &seeded.run).contains(&Anomaly::Breach));
    // Raising both unfences (the live un-fence from a config write).
    let applied = write(PolicyLimits {
        tokens_per_run: Some(5_000_000),
        dollars_per_run_micros: Some(9_000_000),
        breach_action: Some(BreachAction::Block),
        ..PolicyLimits::default()
    });
    assert!(applied.breaches.is_empty());
    assert!(fence.refusal().is_none());
    assert!(latched(&seeded.registry, &seeded.run).is_empty());
    assert!(!anomalies(&seeded.registry, &seeded.run).contains(&Anomaly::Breach));
    // An instance with no open Run has nothing to re-arm.
    let ghost = InstanceName::new("ghost").unwrap();
    let applied = seeded
        .registry
        .rearm_after_config_write(&ghost, ResolveInputs::default(), InstanceContext::default())
        .unwrap();
    assert!(applied.run_events.is_empty() && applied.breaches.is_empty());
}

#[test]
fn an_unsupervised_instance_re_arms_here_but_its_lowering_waits() {
    let seeded = seed();
    let unsupervised = || InstanceContext {
        supervised: false,
        ..context(None, PolicyLimits::default())
    };
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(10),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        unsupervised(),
    )
    .unwrap();
    assert!(outcome.fired.is_empty(), "the supervising engine fires it");
    assert!(outcome
        .notes
        .iter()
        .any(|note| note.contains("does not supervise")));
}

#[test]
fn a_run_limit_belongs_to_its_run_and_a_fresh_run_carries_none() {
    let seeded = seed_with(50, 50, true);
    set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_cumulative: Some(5_000),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    // The Run ends; a fresh start mints a new Run for the same agent.
    let old = seeded.run.clone();
    let fresh = RunId::mint();
    let fresh_row = record(&fresh, true, true, None);
    seeded
        .registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            let mut row = store.run_record(&old)?.unwrap();
            row.status = RunStatus::Ended;
            store.save_run(&row)?;
            store.insert_run(&crate::ports::NewRun {
                record: fresh_row,
                labels: Vec::new(),
                limits: Default::default(),
                identity: None,
            })
        })
        .unwrap();
    assert_eq!(
        seeded.registry.reader().run_limits(&fresh).unwrap(),
        RunLimits::default(),
        "no run_limits row survives into the new Run"
    );
    let view = seeded.registry.run_limits_view(&fresh).unwrap();
    assert_eq!(view.limits, RunLimits::default());
    assert_ne!(view.resolution.sources.tokens_cumulative, LimitSource::Run);
    assert_eq!(view.resolution.tokens.cumulative, None);
}

#[test]
fn a_dry_run_plans_the_same_outcome_and_writes_nothing() {
    let seeded = seed();
    let change = limits(RunLimits {
        token_per_run: Some(10),
        ..RunLimits::default()
    });
    let ctx = context(None, PolicyLimits::default());
    let planned = seeded
        .registry
        .plan_run_limits(&seeded.run, &change, LimitActor::Operator, Some(&ctx))
        .unwrap();
    assert!(planned.dry_run && planned.run_scoped);
    assert_eq!(planned.direction, LimitsDirection::Lowered);
    assert_eq!(planned.fired.len(), 1, "the breach it would fire");
    assert_eq!(
        seeded.registry.reader().run_limits(&seeded.run).unwrap(),
        RunLimits::default()
    );
    assert!(breaches(&seeded.registry, &seeded.run).is_empty());
    // The write does exactly what was planned.
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        change.clone(),
        LimitActor::Operator,
        ctx.clone(),
    )
    .unwrap();
    assert_eq!(outcome.fired, planned.fired);
    // A planned raise reports the latch it would re-arm; a planned no-op
    // reports nothing.
    let raise = limits(RunLimits {
        token_per_run: Some(9_000_000),
        ..RunLimits::default()
    });
    let planned = seeded
        .registry
        .plan_run_limits(&seeded.run, &raise, LimitActor::Operator, Some(&ctx))
        .unwrap();
    assert_eq!(planned.re_armed.len(), 1);
    let planned = seeded
        .registry
        .plan_run_limits(&seeded.run, &change, LimitActor::Operator, Some(&ctx))
        .unwrap();
    assert_eq!(planned.direction, LimitsDirection::Unchanged);
    assert!(planned.fired.is_empty() && planned.re_armed.is_empty());
}

#[test]
fn the_budget_read_names_sources_headroom_and_latches() {
    let seeded = seed();
    set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(400_000),
            dollar_per_run_micros: Some(Micros(3_000_000)),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    let view = seeded.registry.run_limits_view(&seeded.run).unwrap();
    assert_eq!(view.resolution.sources.tokens_per_run, LimitSource::Run);
    assert_eq!(
        view.usage.input_tokens + view.usage.output_tokens,
        1_000_000
    );
    assert!(view.cumulative_usage.is_some());
    assert_eq!(view.latched.len(), 1, "the token breach latched");
    // No Rate resolves through the stored inputs of a fresh read here: the
    // view says the cap cannot fire rather than hiding it.
    let budget = view.budget.expect("armed");
    assert_eq!(budget.per_run_remaining, Some(0));
}

#[test]
fn the_action_row_decides_the_runs_action_and_legacy_rows_keep_theirs() {
    let seeded = seed_with(1, 1, true);
    // A ceiling with no action of its own leaves the action to the next
    // layer (never a silent `warn` from the row's placeholder).
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(9_000),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(
            None,
            PolicyLimits {
                breach_action: Some(BreachAction::Pause),
                ..PolicyLimits::default()
            },
        ),
    )
    .unwrap();
    assert_eq!(outcome.limits.breach_action, None);
    assert_eq!(
        outcome.resolution.sources.breach_action,
        LimitSource::Instance
    );
    assert_eq!(outcome.resolution.action, BreachAction::Pause);
    // Setting then clearing the action round-trips through the store.
    set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            breach_action: Some(BreachAction::Stop),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(
        seeded
            .registry
            .reader()
            .run_limits(&seeded.run)
            .unwrap()
            .breach_action,
        Some(BreachAction::Stop)
    );
    set(
        &seeded.registry,
        &seeded.run,
        RunLimitChange {
            set: RunLimits::default(),
            clear: vec![RunLimitField::BreachAction, RunLimitField::TokensPerRun],
        },
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(
        seeded.registry.reader().run_limits(&seeded.run).unwrap(),
        RunLimits::default()
    );
    // A child opened with ceilings (the 21-2 rows, no action row) keeps the
    // action stored on its ceiling rows.
    let child = RunId::mint();
    let row = record(&child, false, true, Some(&seeded.run));
    seeded
        .registry
        .writer
        .call(move |ctx| {
            ctx.store().insert_run(&crate::ports::NewRun {
                record: row,
                labels: Vec::new(),
                limits: RunLimits {
                    token_per_run: Some(7),
                    ..RunLimits::default()
                },
                identity: None,
            })
        })
        .unwrap();
    assert_eq!(
        seeded
            .registry
            .reader()
            .run_limits(&child)
            .unwrap()
            .breach_action,
        Some(BreachAction::Warn)
    );
}

#[test]
fn a_corrupt_limit_row_is_a_typed_store_error() {
    let seeded = seed_with(1, 1, true);
    let sql = format!(
        "INSERT INTO run_limits (run_id, dimension, scope, limit_value, action, source, set_at) \
         VALUES ('{}', 'tokens', 'per-hour', 5, 'warn', 'operator', 't')",
        seeded.run
    );
    seeded
        .registry
        .writer
        .call(move |ctx| {
            ctx.store().execute_batch_for_test(&sql);
            Ok(())
        })
        .unwrap();
    let err = seeded
        .registry
        .reader()
        .run_limits(&seeded.run)
        .unwrap_err();
    assert!(err.to_string().contains("per-hour"), "{err}");
    let sql = format!(
        "UPDATE run_limits SET scope = 'per-run', action = 'explode' WHERE run_id = '{}'",
        seeded.run
    );
    seeded
        .registry
        .writer
        .call(move |ctx| {
            ctx.store().execute_batch_for_test(&sql);
            Ok(())
        })
        .unwrap();
    let err = seeded
        .registry
        .reader()
        .run_limits(&seeded.run)
        .unwrap_err();
    assert!(err.to_string().contains("explode"), "{err}");
}

#[test]
fn the_budget_jobs_edges_mixed_moves_stored_inputs_and_runs_without_an_agent() {
    use crate::domain::runs::limits;
    for direction in [
        LimitsDirection::Raised,
        LimitsDirection::Lowered,
        LimitsDirection::Cleared,
        LimitsDirection::Set,
        LimitsDirection::Unchanged,
    ] {
        assert!(!direction.as_str().is_empty());
    }
    // Every field sets, clears and round-trips its wire form.
    let full = RunLimits {
        token_per_run: Some(1),
        dollar_per_run_micros: Some(Micros(2)),
        token_cumulative: Some(3),
        dollar_cumulative_micros: Some(Micros(4)),
        breach_action: Some(BreachAction::Stop),
    };
    for field in RunLimitField::ALL {
        assert_eq!(RunLimitField::from_wire(field.as_str()), Some(field));
        let change = RunLimitChange {
            set: full,
            clear: vec![field],
        };
        assert!(change.validate().is_err(), "{field:?} both set and cleared");
        let cleared = RunLimitChange {
            set: RunLimits::default(),
            clear: vec![field],
        }
        .merged_over(&full);
        assert_ne!(cleared, full, "{field:?}");
    }
    assert_eq!(RunLimitField::from_wire("rate"), None);
    assert_eq!(
        limits(RunLimits::default()).merged_over(&full),
        full,
        "an empty set keeps every field"
    );

    // A mixed move (one limit lowered, another cleared) is `set`.
    let seeded = seed();
    set(
        &seeded.registry,
        &seeded.run,
        limits(RunLimits {
            token_per_run: Some(9_000_000),
            dollar_per_run_micros: Some(Micros(9_000_000)),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    let outcome = set(
        &seeded.registry,
        &seeded.run,
        RunLimitChange {
            set: RunLimits {
                token_per_run: Some(8_000_000),
                ..RunLimits::default()
            },
            clear: vec![RunLimitField::DollarsPerRun],
        },
        LimitActor::Operator,
        context(None, PolicyLimits::default()),
    )
    .unwrap();
    assert_eq!(outcome.direction, LimitsDirection::Set);

    // With no caller context the Run's stored inputs resolve it, and a
    // `block` refusal names the Metering Source from the wire path.
    let no_wire = seed_with(1, 1, false);
    let planned = no_wire
        .registry
        .plan_run_limits(
            &no_wire.run,
            &limits(RunLimits {
                token_per_run: Some(5),
                ..RunLimits::default()
            }),
            LimitActor::Operator,
            None,
        )
        .unwrap();
    assert!(planned.dry_run);
    let err = no_wire
        .registry
        .plan_run_limits(
            &no_wire.run,
            &limits(RunLimits {
                breach_action: Some(BreachAction::Block),
                ..RunLimits::default()
            }),
            LimitActor::Operator,
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("self-reported"), "{err}");
    let wired = seed();
    let child = add_child(&wired, true);
    let err = wired
        .registry
        .plan_run_limits(
            &child,
            &limits(RunLimits {
                token_per_run: Some(5),
                breach_action: Some(BreachAction::Stop),
                ..RunLimits::default()
            }),
            LimitActor::Operator,
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("engine-observed"), "{err}");

    // The budget read with an unreadable configuration says so and falls
    // back to the stored inputs.
    let view = limits::limits_view(
        wired.registry.reader(),
        &wired.run,
        Some(Err("bad toml".into())),
    )
    .unwrap();
    assert!(view.notes.iter().any(|note| note.contains("bad toml")));
    let view = limits::limits_view(wired.registry.reader(), &wired.run, None).unwrap();
    assert!(view.cumulative_usage.is_some());

    // A Run with no agent: nothing to re-arm, no cumulative usage.
    let orphan = RunId::mint();
    let mut row = record(&orphan, false, true, None);
    row.instance = None;
    wired
        .registry
        .writer
        .call(move |ctx| {
            ctx.store().insert_run(&crate::ports::NewRun {
                record: row,
                labels: Vec::new(),
                limits: Default::default(),
                identity: None,
            })
        })
        .unwrap();
    let outcome = set(
        &wired.registry,
        &orphan,
        limits(RunLimits {
            token_per_run: Some(5),
            ..RunLimits::default()
        }),
        LimitActor::Operator,
        InstanceContext::default(),
    )
    .unwrap();
    assert!(outcome.fired.is_empty() && outcome.re_armed.is_empty());
    let planned = wired
        .registry
        .plan_run_limits(
            &orphan,
            &limits(RunLimits {
                token_per_run: Some(1),
                ..RunLimits::default()
            }),
            LimitActor::Operator,
            None,
        )
        .unwrap();
    assert!(planned.fired.is_empty());
    let view = wired.registry.run_limits_view(&orphan).unwrap();
    assert!(view.cumulative_usage.is_none());
}
