# Deprecated crate shims (one-shot)

Three final, one-time publishes that deprecate the pre-rename crate names
(ratified 2026-09-16, spec docs/proposals/hekma-migration-spec.md):

| Crate | Final version | Re-exports |
|---|---|---|
| `ktesio-engine` | 0.3.1 | `hekma-engine` 0.4 |
| `ktesio-adapter-api` | 0.1.1 | `hekma-adapter-api` 0.2 |
| `ktesio-adapters-hermes` | 0.1.1 | `hekma-adapters-hermes` 0.2 |

Each shim exists ONLY to point crates.io visitors at the renamed crate and
to keep existing `use ktesio_*::…` paths compiling after `cargo update`
(the re-exported types ARE the new crate's types — one crate instance, no
duplication). They publish ONCE, each on Islam's explicit go, AFTER the
`hekma-*` libraries are live (cargo publish needs the dependency on the
registry), and BEFORE the v0.8.0 tag. Never publish another version under
these names. The CLI crate `ktesio` is deliberately NOT shimmed: a
bin-less final version would break old `kt` binaries' harmless
`cargo install ktesio --force` self-update reinstall.

These are NOT workspace members on purpose (standalone `[workspace]`
tables) — they stay outside the boundary gate and the root lockfile. The
CI semver job compile-checks them against the PUBLISHED hekma-* crates
once those exist (arm-on-publish, like the both-graph consumer).
