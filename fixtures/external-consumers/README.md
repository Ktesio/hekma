# External consumer fixtures

Out-of-tree consumers proving the Hekma rename (v0.8.0) is surface-safe
for real hosts. None of these are workspace members — they build exactly
the way a third-party host builds: against the crate as published or as a
path dependency, from OUTSIDE the workspace.

- `engine-consumer/` — the embedding-facade lifecycle (open → register →
  configure → subscribe → start → observe → stop), derived from the
  engine's own `embedding-quickstart` example with the dependency aliased
  to the neutral name `engine`. Built by
  `scripts/rename_surface_check.sh` against BOTH the baseline rev's
  `ktesio-engine` (freeze commit 49da96b) and HEAD's `hekma-engine` —
  the rename is surface-compatible iff both builds (and smoke runs)
  succeed.
- `adapter-api-consumer/` — the Adapter Contract surface
  (`Manifest::from_toml_str` + `validate` + projections), checked the
  same way against the contract-v1 freeze (4119db3).
- `both-graph/` — depends on the PUBLISHED `ktesio-engine` 0.3.0 AND
  `hekma-engine` in ONE dependency graph, proving a host mid-migration
  keeps building. Armed by CI once `hekma-engine` is on crates.io
  (the semver job's 404/200 probe); builds from the registry, not paths.

No compatibility claim ever comes from a skipped check: if a variant
cannot build, the gate fails loudly.
