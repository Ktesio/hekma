//! External-consumer fixture: the PUBLISHED `ktesio-engine` (frozen line,
//! 0.3.0) and the renamed `hekma-engine` must resolve TOGETHER in one
//! external dependency graph. Dependency aliases alone prove nothing —
//! this crate depends on BOTH registry packages at once and references
//! each crate's embedding facade, proving a host mid-migration (old
//! dependency + new dependency in one lockfile) keeps building.
//!
//! Armed by CI only once `hekma-engine` exists on crates.io (the semver
//! job's 404/200 probe); before that the check skips with a notice.

fn main() {
    // Both crates export the embedding facade under their own names;
    // referencing the item proves resolution, compilation, and linkage of
    // BOTH crates in one graph.
    let _old = ktesio_engine::Engine::open;
    let _new = hekma_engine::Engine::open;
    println!("both-graph consumer ok");
}
