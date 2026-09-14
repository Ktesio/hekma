---
title: '11-2 Config & secrets batch — atomic writes, env-shadow warning, secret-to-flag steer, leading-dash values'
type: 'bugfix'
created: '2026-09-11'
status: 'done'
review_loop_iteration: 0
baseline_commit: 'b4456c16454d9468f6f4523928d17ecfbb016fc1'
context:
  - '{project-root}/_bmad-output/implementation-artifacts/epic-11-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Six retro items (AI-24/26/27/28/33/39) record config-surface robustness gaps: `set_config` and the config-file renderer write agents' config files non-atomically (a crash mid-write truncates them), a mapped env target silently shadows a base-launch env var with no trace, a `secret:NAME` value mapped to a FLAG target puts a cleartext secret into argv with no warning at set time or start time, and `kt agent config set` rejects leading-dash values at the clap parse layer.

**Approach:** One shared temp-file + rename helper makes all durable config writes atomic; a shadow-diff diagnostic names env vars the mapping overwrote; the existing `secret:` predicate warns at set time (steering to env/file targets) and at start time (runtime enforcement riding the existing audit trail); and `value` accepts leading dashes so `--` becomes optional rather than mandatory.

## Boundaries & Constraints

**Always:** Every fix ships with a test. The atomic-write helper lives in ktesio-engine (crate-internal), writes the temp file in the TARGET's directory (same-FS rename), and cleans up temp residue on failure. The AI-27 precedence stays exactly as-is (config overrides base, last-write-wins) — only a diagnostic is added. The AI-33/39 warning is WARN-ONLY (no rejection, no exit-code change) and reuses the existing `secret:` predicate; the set-time warning travels through the return path the CLI already prints to stderr. Docs updated in the same change: `docs/commands.md` config-set section (atomic write note, warning line, leading-dash note) and `docs/architecture.md` config seam (~104) + secrets boundary (~118). Full battery green.

**Ask First:** Any change that makes secret-to-FLAG a hard error (rejection semantics were never ratified). Any new dependency (no tempfile crate — std-only helper).

**Never:** Do not change mapping precedence or `render_flag_args`' argv shape. Do not touch the audit's runtime REJECTION of secrets in other channels — only the FLAG-target warning is added. Do not adopt the atomic helper beyond config-family writes named in the tasks (registration/snapshot writes stay as-is unless trivially adjacent).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Atomic set_config | `config set` succeeds or fails; process may die mid-write | instance `config.toml` always holds either the old or the new bytes; no `.tmp` residue in the Agent Home on any path | rename failure surfaces the existing `ConfigError` mapping |
| Atomic config-file render | start renders a native TOML file target | file lands byte-identical to the render, atomically; a failed write leaves the previous native file unchanged | start fails before the `starting` transition (existing semantics) |
| Env target shadows base-launch var | manifest `[lifecycle.start].env FOO=a`; config maps key → env `FOO=b` | config wins (unchanged precedence) AND a diagnostic names the shadowed var (`FOO`) at start | not silent (AI-27) |
| No shadow | mapped env targets are all new names | no diagnostic | clean path stays quiet |
| secret:NAME → FLAG target at set time | `config set svc <flag-targeted-key> secret:MY_SECRET` | set SUCCEEDS and a stderr warning names the key + the flag-target leak risk + the env/file alternative (AI-33/39) | warn-only |
| secret:NAME → FLAG target at start | instance starts with that mapping | a start-path diagnostic names the cleartext-into-argv fact (rides the audit trail) | warn-only (AI-39 runtime half) |
| Leading-dash value | `kt agent config set svc key -x` (no `--`) | value accepted verbatim (clap `allow_hyphen_values`); `--` form still works | no exit-2 parse error (AI-26) |

</frozen-after-approval>

## Code Map

- `crates/ktesio-engine/src/domain/registry.rs` -- `set_config` ~979: validate ~989 → `set_dotted` ~997 → `toml::to_string_pretty` ~999 → NON-ATOMIC `std::fs::write` ~1005 (AI-24 target); failure-injection test pattern ~2719 (directory at target path); set_config test neighbors ~2261/~2427; `resolve_secrets` ~1070 (`secret:` predicate to reuse); `adapter_launch_facts` + `resolve_config_mapping` → `mapping.target(key)` pattern via `memory_key_declared` ~741 (how to learn a key's target at set time)
- `crates/ktesio-engine/src/adapter/mod.rs` -- `apply_config_mapping` ~500: pass-through `agent.TAIL` env insert ~523–529; `ConfigTarget::Env` insert ~536–538 (AI-27 shadow site — capture the base `launch.env` key set before, diff after); Flag arm ~539–558 with the `TODO(follow-up)` comment ~540–553 (AI-39 runtime warn hook); existing test: secret-mapped-to-FLAG lands cleartext ~1537; `write_config_file` ~638, NON-ATOMIC write ~654 (AI-28 target); FileRender failure tests ~1747/~1775 (blocked-path injection pattern); `resolve_start_launch` ~215–222 (`StartLaunch.env`)
- `crates/ktesio-engine/src/domain/supervisor.rs` -- start path: launch resolved ~810–828, `apply_config_mapping` call ~1007 (shadow-diagnostic emission point via `emit_diagnostic` ~737; precedent `memory_delivery_notice` ~991); pure-helper home for the shadow diff next to the ~3989 `apply_config_mapping` direct-call test
- `crates/kt/src/main.rs` -- `ConfigCommands::Set` ~266–276: `value` positional ~275 needs `#[arg(allow_hyphen_values = true)]` (AI-26; in-repo precedent: `send`'s text arg ~184)
- `crates/kt/src/cli/agent.rs` -- `config_set` ~1482: prints `ui::success` on Ok — the stderr warning needs either an augmented return or a pre-success stderr line here; test neighbors in `crates/kt/tests/agent_cli.rs`: Story 2-1 config block ~2892, `config_set_then_get_shows_the_value_on_stdout` ~2895, unknown-key ~3065, hyphen-leading send value ~1341 (the AI-26 pattern-match), pass-through round-trip ~3108
- `crates/ktesio-engine/src/engine.rs` -- async `set_config` ~1034 + `Blocking::set_config` ~1482 (warning return path, if shaped through the facade)
- Helper home -- a small std-only atomic-write util in ktesio-engine (e.g. `src/paths.rs` or a domain io util): temp file in target's parent + `std::fs::rename` + best-effort temp cleanup on error; NO tempfile dependency
- `docs/commands.md` -- config set section ~296–303 (atomic note, warning line, leading-dash note); `docs/architecture.md` ~104 (seam: precedence sentence + "atomically"), ~118 (secrets boundary: warn at set + start)

## Tasks & Acceptance

**Execution:**
- [ ] `crates/ktesio-engine/src/paths.rs` (or equivalent) -- AI-24: the shared `write_atomically(target_path, bytes)` helper: write `<target>.tmp-<pid>` in the same directory, `fs::rename` over the target, remove the temp on error; unit test covers success, rename failure (directory at target), and zero temp residue -- one helper, std-only
- [ ] `crates/ktesio-engine/src/domain/registry.rs` -- AI-24: `set_config`'s ~1005 write goes through the helper; keep the error mapping (wrap rename failure as today's Io/MalformedLayer shape); test: forced failure leaves the old bytes intact and no temp file behind -- durable config writes
- [ ] `crates/ktesio-engine/src/adapter/mod.rs` -- AI-28: `write_config_file`'s ~654 write goes through the helper; test: on injected write failure the previous native file is unchanged and no temp remains (pattern-match ~1747) -- native configs never truncate
- [ ] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-27: capture `launch.env`'s key set before ~1007; after apply, diff and emit ONE diagnostic naming shadowed vars (a small pure `shadowed_env_keys(before, after)` helper + unit test next to ~3989); precedence untouched -- shadowing becomes visible
- [ ] `crates/ktesio-engine/src/domain/registry.rs` -- AI-33 set-time: after `validate_write` ~989, if the value starts with `secret:` AND the adapter maps `key` to a Flag target (reuse the ~741 pattern), produce a warning; surface it through `set_config`'s return (augment with a warnings vec or a sibling return — the minimal shape that lets the CLI print it to stderr without breaking existing callers) -- steer at set time
- [ ] `crates/kt/src/cli/agent.rs` -- AI-33: `config_set` prints the warning to stderr when present (warn-only; success output unchanged); kt test pins the stderr warning on a secret→flag set -- operator sees it
- [ ] `crates/ktesio-engine/src/adapter/mod.rs` -- AI-39 runtime: resolve the ~549 TODO — the Flag arm reports secret-into-flag facts back (return value or out-param, crate-internal), and `start_inner` emits ONE diagnostic naming the keys delivering cleartext args; extend the ~1537 test to pin the report -- runtime enforcement rides the audit trail
- [ ] `crates/kt/src/main.rs` -- AI-26: `#[arg(allow_hyphen_values = true)]` on the `value` positional ~275; kt test: `config set svc key -x` round-trips verbatim (mirror ~1341's hyphen test) -- leading-dash values accepted
- [ ] `docs/commands.md` + `docs/architecture.md` -- atomic-write note on `config set` + file targets; the AI-27 precedence sentence ("config-mapped env overrides a same-named base-launch var; the shadow is reported on stderr"); the secrets boundary sentence (warn at set + start, env/file preferred); the leading-dash note -- doc currency
- [ ] Full battery at story end: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py` -- all green

**Review-1 patches (loop 1, 2026-09-11 — 11 applied; 2 defers recorded):**
- [x] `crates/ktesio-engine/src/paths.rs` -- temp name collision-safe within one process: `<file>.tmp-<pid>-<tid>-<seq>` (thread id + the `TEMP_WRITE_SEQ` AtomicU64 alongside the pid); test: two concurrent same-target writers both succeed, final bytes are one of them, no residue
- [x] same helper -- `sync_all()` the temp BEFORE the rename; doc comment states the honest boundary (process-death atomicity guaranteed; power-loss durability best-effort at file level — no portable directory fsync)
- [x] same helper -- preserve an existing target's Unix permissions across the flip (cfg(unix) mode copy via the `backends/` cfg home; no-op where mode bits do not exist); test: chmod 600 config, re-set through the helper AND the full set_config seam, mode survives
- [x] same helper -- Windows rename-over-open-file sharing violation: one ~100ms backoff + single retry, then the honest error; doc caveat (the write FAILS where the old in-place write silently overwrote)
- [x] `crates/ktesio-engine/src/domain/registry.rs` -- `materialize_home`'s instance config.toml write goes through `write_atomically` (same file, same contract); failure-shape test strengthened at the registration seam (typed Io naming config.toml, row rolled back, no partial home)
- [x] supervisor.rs env-shadow diagnostic -- reworded to "launch environment variable(s)" (no "base-launch"/template claim; the snapshot may carry other launch env); unit test pins the fragment
- [x] supervisor.rs tests -- the secret-steering test's process-global env var gets a restore-on-drop guard (survives assertion failure)
- [x] adapter/mod.rs tests -- `apply_report_is_empty_without_a_secret_flag_combination` gains the promised secret-on-unmapped-key quiet half
- [x] registry.rs `secret_flag_steering` -- comment records the warning is best-effort against the CURRENT manifest declaration (post-registration edits can make it stale; start-time reads the registration snapshot)
- [x] docs/commands.md -- `kt agent start` section names both new start-time diagnostics; README config section verified current (no change needed)
- [x] this task list -- these entries
- DEFERRED (orchestrator records): crash-litter sweep of stale `.tmp-<pid>` files (cross-engine safety argument needed); the adapter-snapshot and effective-snapshot writes (outside the config.toml contract this story names)
- REJECTED (no action): #[must_use] warnings newtype; flag-looking-value warning; removal-detection in the shadow diff; changelog/version-bump; predicate-agreement test

**Acceptance Criteria:**
- Given any `set_config` or config-file render, when the process dies at ANY point during the write, then the target file holds either the complete old or complete new bytes and no temp residue remains
- Given a mapped env target whose name exists in the base launch env, when the instance starts, then the config value wins AND one diagnostic names the shadowed variable
- Given a `secret:` value set on a flag-targeted key, when it is set and again when the instance starts, then a warning names the cleartext-into-argv risk each time — and the command/start still succeeds (warn-only)
- Given `kt agent config set svc key -x` without `--`, when it runs, then the value round-trips verbatim (exit 0)
- Given the full battery, when run at story end, then all green

## Spec Change Log

- 2026-09-11 — Approval note: approved on autopilot per the standing per-epic workflow; the frozen block restates the Islam-approved sprint change proposal 2026-09-10 §3 Story 11-2 table. Channel decision recorded (AI-33/39 set-time warning): the warning travels in `set_config`'s return value (the minimal crate-internal shape) so the CLI can print it to stderr synchronously — `emit_diagnostic` is supervisor-only and the registry has no sink. Continuity: 11-3 touched `supervisor.rs` start_inner's override branch and 11-1 touched `emit_diagnostic` call sites — build on the current tree.

## Verification

**Commands:**
- `cargo fmt --all --check` -- expected: no diffs
- `cargo clippy --workspace --all-targets -- -D warnings` -- expected: zero warnings
- `cargo test --workspace --all-targets` -- expected: all pass
- `python3 scripts/check_docs.py` -- expected: pass

## Suggested Review Order

**The atomic-write core (AI-24/28)**

- The helper: collision-safe temp name, fsync-before-rename, unix mode preservation, Windows retry — the durability contract and its honest power-loss boundary.
  [`paths.rs:59`](../../crates/ktesio-engine/src/paths.rs#L59)
- set_config through the helper; error mapping unchanged.
  [`registry.rs:979`](../../crates/ktesio-engine/src/domain/registry.rs#L979)
- Registration adopts the same helper — the contract's config.toml is atomic on BOTH write paths.
  [`registry.rs:472`](../../crates/ktesio-engine/src/domain/registry.rs#L472)
- Native config-file render through the helper.
  [`adapter/mod.rs:638`](../../crates/ktesio-engine/src/adapter/mod.rs#L638)
- The deterministic failure-injection tests (unix-gated integration file).
  [`atomic_config_writes.rs:1`](../../crates/ktesio-engine/tests/atomic_config_writes.rs#L1)

**Env-shadow visibility (AI-27)**

- The shadow diff (value-CHANGED is the condition) + the one diagnostic.
  [`supervisor.rs:4170`](../../crates/ktesio-engine/src/domain/supervisor.rs#L4170)
- Emission at the start seam; e2e positive/negative pins.
  [`supervisor.rs:1007`](../../crates/ktesio-engine/src/domain/supervisor.rs#L1007)

**Secret→flag steering (AI-33/39)**

- Set-time steering (best-effort, current-manifest based) via the warnings-vec return.
  [`registry.rs:1050`](../../crates/ktesio-engine/src/domain/registry.rs#L1050)
- Runtime report through apply_config_mapping (the resolved ~549 TODO) + start diagnostic.
  [`adapter/mod.rs:539`](../../crates/ktesio-engine/src/adapter/mod.rs#L539)
- CLI stderr printing; leading-dash value acceptance.
  [`agent.rs:1482`](../../crates/kt/src/cli/agent.rs#L1482)

**Leading-dash values (AI-26)**

- allow_hyphen_values on the value positional (send-precedent) + round-trip tests.
  [`main.rs:275`](../../crates/kt/src/main.rs#L275)

**Docs**

- commands.md config-set + start sections; architecture.md seam + secrets boundary.
  [`commands.md:296`](../../docs/commands.md#L296)
