//! The verb × Control Depth table as a conformance-tested fixture (PRD FR-49:
//! "Every cell is covered by a conformance test").
//!
//! `fixtures/verb-depth.json` pins every cell: the behavior FR-49 states and
//! a status —
//!
//! * `tested` — delivered today and probed live below, against a real engine
//!   and a real `fake_agent` process;
//! * `covered` — delivered, and exercised by the engine's own suites rather
//!   than a probe here (the wire column's metered paths need model traffic);
//! * `pending` — not delivered yet; a later change flips it as it builds the
//!   verb at that depth.
//!
//! The test asserts the fixture equals the engine's `verb_cell` table (and
//! that `tested`/`covered` is exactly the engine's `available`), then runs
//! every `tested` probe. Regenerate after a deliberate table change with
//! `HEKMA_UPDATE_VERB_DEPTH=1 cargo test -p hekma-conformance --test verb_depth`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hekma_engine::{
    verb_cell, AdapterRef, BreachAction, ChildRunRequest, ControlDepth, Engine, RunError,
    RunParent, RunVerb, RUN_VERBS,
};
use serde::{Deserialize, Serialize};

/// The fixture's columns: the four depths plus the no-wire variants FR-49
/// names (`supervised (no wire)`, and a child Run inside such a process).
const COLUMNS: [(&str, ControlDepth, bool); 6] = [
    ("observed", ControlDepth::Observed, false),
    ("wire", ControlDepth::Wire, true),
    ("announced", ControlDepth::Announced, true),
    ("announced-no-wire", ControlDepth::Announced, false),
    ("supervised", ControlDepth::Supervised, true),
    ("supervised-no-wire", ControlDepth::Supervised, false),
];

/// The cells probed live below.
const PROBED: [(RunVerb, &str); 12] = [
    (RunVerb::ListUsage, "supervised"),
    (RunVerb::ListUsage, "supervised-no-wire"),
    (RunVerb::ListUsage, "announced-no-wire"),
    (RunVerb::Logs, "supervised-no-wire"),
    (RunVerb::BudgetsWarn, "supervised-no-wire"),
    (RunVerb::BudgetsWarn, "announced-no-wire"),
    (RunVerb::PauseResume, "supervised"),
    (RunVerb::PauseResume, "supervised-no-wire"),
    (RunVerb::Stop, "supervised-no-wire"),
    (RunVerb::SendAttach, "supervised-no-wire"),
    (RunVerb::BreachActionPauseStopBlock, "supervised-no-wire"),
    (RunVerb::BreachActionPauseStopBlock, "announced-no-wire"),
];

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fixture {
    fixture: String,
    version: u32,
    cells: Vec<Cell>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Cell {
    verb: String,
    column: String,
    behavior: String,
    status: String,
    note: String,
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/verb-depth.json")
}

/// The table as the engine states it, with each cell's status.
fn expected() -> Fixture {
    let mut cells = Vec::new();
    for verb in RUN_VERBS {
        for (column, depth, wire) in COLUMNS {
            let cell = verb_cell(verb, depth, wire);
            let probed = PROBED.contains(&(verb, column));
            let status = match (cell.available, probed) {
                (true, true) => "tested",
                (true, false) => "covered",
                (false, _) => "pending",
            };
            cells.push(Cell {
                verb: verb.as_str().to_string(),
                column: column.to_string(),
                behavior: cell.behavior.as_str().to_string(),
                status: status.to_string(),
                note: cell.note.to_string(),
            });
        }
    }
    Fixture {
        fixture: "hekma-verb-depth".to_string(),
        version: 1,
        cells,
    }
}

#[test]
fn the_fixture_is_the_engine_table() {
    let expected = expected();
    let path = fixture_path();
    if std::env::var_os("HEKMA_UPDATE_VERB_DEPTH").is_some() {
        let mut json = serde_json::to_string_pretty(&expected).unwrap();
        json.push('\n');
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json).unwrap();
    }
    let committed: Fixture =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the fixture exists"))
            .expect("the fixture parses");
    assert_eq!(committed, expected, "the fixture drifted from verb_cell");
    assert_eq!(committed.cells.len(), RUN_VERBS.len() * COLUMNS.len());
    // A probed cell must be one the engine delivers.
    for (verb, column) in PROBED {
        let (_, depth, wire) = COLUMNS.iter().find(|(c, ..)| *c == column).unwrap();
        assert!(
            verb_cell(verb, *depth, *wire).available,
            "{} at {column} is probed but not delivered",
            verb.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// The live probes
// ---------------------------------------------------------------------------

/// One `fake_agent` instance, self-reported or engine-observed.
fn register(engine: &Engine, dir: &Path, name: &str, observed: bool, args: &[&str]) {
    let exec = hekma_conformance::fake_agent_bin();
    let args = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = if observed {
        "engine-observed"
    } else {
        "self-reported"
    };
    let manifest = dir.join(name);
    std::fs::create_dir_all(&manifest).unwrap();
    std::fs::write(
        manifest.join("adapter.toml"),
        format!(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"{name}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args}]\n\n\
             [capabilities.pause]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\n\
             windows = \"best-effort\"\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\n\
             windows = \"guaranteed\"\n\n[metering]\nsource = \"{source}\"\n"
        ),
    )
    .unwrap();
    let facade = engine.blocking();
    facade
        .register_with_adapter(name, &AdapterRef::Manifest(manifest))
        .unwrap();
    if observed {
        facade
            .set_config(name, "metering.upstream_base_url", "http://127.0.0.1:9")
            .unwrap();
        // Story 22-2: an engine-observed agent starts only when Hekma holds its
        // provider key (FR-19); this metering test holds one with custody off.
        std::env::set_var("HEKMA_TEST_PROVIDER_KEY", "sk-test-held-provider-key");
        facade
            .set_config(name, "provider.api_key", "secret:HEKMA_TEST_PROVIDER_KEY")
            .unwrap();
        facade.set_config(name, "provider.custody", "off").unwrap();
    }
}

fn open_run_id(engine: &Engine, instance: &str) -> String {
    let child = engine
        .blocking()
        .open_child_run(ChildRunRequest::under(RunParent::Instance(
            instance.to_string(),
        )))
        .unwrap();
    child
        .record
        .parent_run_id
        .expect("a child names its parent")
        .to_string()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn every_tested_cell_behaves_as_the_table_states() {
    let state = tempfile::tempdir().unwrap();
    let manifests = tempfile::tempdir().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    register(
        &engine,
        manifests.path(),
        "nowire",
        false,
        &[
            "--emit-usage",
            "1",
            "--usage-input-tokens",
            "40",
            "--echo-stdin",
            "--linger-ms",
            "600000",
        ],
    );
    register(
        &engine,
        manifests.path(),
        "wired",
        true,
        &["--linger-ms", "600000"],
    );
    // Budgets with `warn` (honored): a 10-token per-run ceiling the emitted
    // usage crosses.
    facade
        .set_config("nowire", "budget.tokens.per_run", "10")
        .unwrap();
    facade
        .set_config("nowire", "budget.breach_action", "warn")
        .unwrap();
    facade.start("nowire").unwrap();
    facade.start("wired").unwrap();

    // list-usage — supervised, both columns: the instance row and its Run.
    for name in ["nowire", "wired"] {
        facade.fleet_entry(name).unwrap();
        let owner = open_run_id(&engine, name);
        let run = facade.run(&owner).unwrap();
        assert_eq!(run.record.control_depth, ControlDepth::Supervised);
        assert_eq!(run.record.wire_path, name == "wired");
    }

    // budgets-warn — supervised (no wire): the breach is recorded, nothing
    // stops.
    wait_for("the warn breach", || {
        !facade.budget_breach_events("nowire").unwrap().is_empty()
    });
    let breach = &facade.budget_breach_events("nowire").unwrap()[0];
    assert_eq!(breach.action, BreachAction::Warn);
    assert_eq!(
        facade.instance_status("nowire").unwrap().instance.state,
        hekma_engine::LifecycleState::Running
    );

    // list-usage + budgets-warn — announced (no wire): a child Run reads its
    // own usage, and its ceiling takes `warn`.
    let mut request = ChildRunRequest::under(RunParent::Instance("nowire".to_string()));
    request.limits.token_per_run = Some(5);
    let child = facade.open_child_run(request).unwrap();
    assert_eq!(child.record.control_depth, ControlDepth::Announced);
    assert!(!child.record.wire_path);
    let read = facade.run(child.record.run_id.as_str()).unwrap();
    assert_eq!(read.usage.input_tokens, 0);
    assert_eq!(read.limits.action(), BreachAction::Warn);

    // breach-action pause/stop/block — announced (no wire): refused at
    // configuration, naming the Metering Source.
    for action in [BreachAction::Pause, BreachAction::Stop, BreachAction::Block] {
        let mut request = ChildRunRequest::under(RunParent::Instance("nowire".to_string()));
        request.limits.breach_action = Some(action);
        match facade.open_child_run(request) {
            Err(err @ RunError::ActionUnavailable { .. }) => {
                assert!(err.to_string().contains("self-reported"), "{err}");
            }
            other => panic!("expected a refusal for {action:?}, got {other:?}"),
        }
    }

    // breach-action pause/stop/block — supervised (no wire): pause and stop
    // execute, block is refused at configuration.
    assert!(facade
        .set_config("nowire", "budget.breach_action", "block")
        .is_err());

    // logs — supervised (no wire): process output is readable.
    facade.read_agent_log("nowire").unwrap();

    // send — supervised (no wire): per the Capability Declaration.
    facade.send_input("nowire", "hello").unwrap();

    // pause/resume — supervised, both columns: the Adapter pause, and the
    // Run follows (held, then active).
    for name in ["nowire", "wired"] {
        let owner = open_run_id(&engine, name);
        facade.pause(name).unwrap();
        assert_eq!(
            facade.run(&owner).unwrap().record.status,
            hekma_engine::RunStatus::Held
        );
        facade.resume(name).unwrap();
        assert_eq!(
            facade.run(&owner).unwrap().record.status,
            hekma_engine::RunStatus::Active
        );
    }

    // stop — supervised (no wire): the process stops and the Run ends.
    let owner = open_run_id(&engine, "nowire");
    facade.stop("nowire", Some(Duration::from_secs(5))).unwrap();
    assert_eq!(
        facade.run(&owner).unwrap().record.status,
        hekma_engine::RunStatus::Ended
    );
    let _ = facade.stop("wired", Some(Duration::from_secs(5)));
}
