//! AD-18 Step 3: the registry lock covers the store, not the filesystem.
//! Removing an agent deletes its row under the lock and its Agent Home after
//! the lock drops, so a slow delete never queues another command behind it.

use std::path::PathBuf;
use std::time::Instant;

use hekma_engine::{Engine, RemoveDisposition};
use tempfile::TempDir;

#[test]
fn removing_an_agent_deletes_its_home_after_the_registry_lock_drops() {
    let state = TempDir::new().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    let agent = facade.register("bulky", "mock").unwrap();
    let home = PathBuf::from(&agent.agent_home);
    // A home whose recursive delete takes measurable time.
    for dir in 0..40 {
        let dir = home.join(format!("d{dir}"));
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..100 {
            std::fs::write(dir.join(format!("f{file}")), b"x").unwrap();
        }
    }
    engine.set_lock_timing(true);
    let started = Instant::now();
    facade
        .remove("bulky", RemoveDisposition::Delete, false)
        .unwrap();
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(!home.exists(), "the home is deleted");
    assert!(facade.list().unwrap().is_empty(), "the row is deleted");
    let held_ms = engine
        .lock_hold_stats()
        .into_iter()
        .filter(|stats| stats.lock == "registry")
        .map(|stats| stats.max_ms)
        .fold(0.0, f64::max);
    assert!(
        held_ms < total_ms / 2.0,
        "the registry was held {held_ms:.1} ms of a {total_ms:.1} ms remove: the home delete \
         ran under it"
    );
}

#[test]
fn a_retained_home_survives_the_split_remove() {
    let state = TempDir::new().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    let agent = facade.register("kept", "mock").unwrap();
    facade
        .remove("kept", RemoveDisposition::Retain, false)
        .unwrap();
    assert!(PathBuf::from(&agent.agent_home).is_dir());
    assert!(facade.list().unwrap().is_empty());
}
