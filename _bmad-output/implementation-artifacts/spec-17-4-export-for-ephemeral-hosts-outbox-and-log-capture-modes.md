---
title: 'Export for ephemeral hosts — the outbox and log capture modes'
type: 'feature'
created: '2026-10-19'
status: 'done'
review_loop_iteration: 0
baseline_commit: '3cc3d611aa76076c55d691b943a19b9df46c8511'
context:
  - '_bmad-output/implementation-artifacts/epic-17-context.md'
  - '_bmad-output/implementation-artifacts/spec-17-3-control-api-v0.md'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§Story 15.4 → renumbered 17.4, :370-376)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-14 + 2026-09-30 amendment :215-216; AD-12 + amendment :213-214; AD-13; AD-20 :178-200)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** A Host on an ephemeral disk loses the engine's event record when the container dies. Today's pull is per-instance/per-family (`resync_events`) with no global order and no acknowledgement; and a Host whose agent output may carry user content has no way to keep that content off disk — `agent.log` grows unbounded (`crates/hekma-engine/src/domain/supervisor/mod.rs:335`).

**Approach:** An engine-owned global outbox (AD-14 amendment): one durable append-only journal at the state base carrying the transition, breach and usage families under ONE monotonically increasing sequence, written at the three existing commit sites; the Host pulls it by cursor over an additive Control-API v0.x route shaped like 17-3's per-family pull and ACKNOWLEDGES a seq to advance a durable consumer cursor — un-acked events survive for re-pull; retention is bounded like the log families' rotation discipline and honestly surfaced. Per-family `resync_events` is unchanged. Log capture gains modes `full` (default, byte-identical), `metadata`, `off` (AD-12 amendment), threaded serve→engine like 17-2's `metering_listen_port`; `agent.log` gains `output.log`'s bound. The state dir is already placeable on a Host-chosen volume (`--state-dir`, 17-2) — this story verifies and documents that; no invented surface.

## Tasks & Acceptance

**Execution:**
^- [x] `crates/hekma-engine/src/domain/outbox.rs` — NEW: `OutboxCursor` (`{seq: u64}`, snake_case, `schema_version` on payloads per AD-14); `OutboxEntry {seq, event: EngineEvent}` reusing the EXACT three bus wrappers (no new event kinds — the `resync.rs:96-100` boundary); the journal writer (append-only JSON-Lines at a new `EnginePaths::outbox_journal()`) with resync-style torn-tail tolerance; seq assigned under the registry lock at the three commit sites — transitions (`supervisor/mod.rs:1304`), breaches (`:1663`), usage (post-`Inserted` in `ingest_usage`, `usage.rs:618`, before the `:650` publish); next-seq re-derived from the journal tail at engine open (survives restarts). An outbox-append failure NEVER fails the family commit — surfaced diagnostic (AI-18); the family stores stay the truth, `resync_events` the heal path. Tests: cross-family seq monotonicity, restart re-derivation, torn-tail surfaced, append-failure degradation.
^- [x] Engine facade — additive: `Engine::export_events(after: Option<OutboxCursor>, max: u32)` → `ExportBatch {schema_version, events, cursor, torn_tail_skipped, retained_from}` (bounded batch: `max` default + cap constants); `Engine::ack_export(seq)` persisting the consumer cursor via `paths::write_atomically` (`paths.rs:133`) to a new `EnginePaths::outbox_ack()`. Cursor semantics mirror the resync contract (`resync.rs:84-92`): default pull resumes from the ACK cursor; an explicit cursor re-pulls; a cursor BELOW `retained_from` (rotated away) is a typed `EngineError::Log` naming both counts — never a clamp, never a new error arm; ack past the committed tail is the same refusal. `Blocking` siblings. Tests: pull/ack idempotence, below-floor refusal, ack durability across reopen.
^- [x] Host projection — additive v0.x routes beside the per-family pull (`control/routes.rs:347`): `GET /v0/export?cursor={"seq":n}&limit=n` and `POST /v0/export/ack {"seq":n}`, behind the existing auth layer, versioned envelope, `schema_version`-stamped DTOs; extend the error mapper (below-floor/ack-past-tail → 409 `cursor_invalid`). Tests in `tests/control_api.rs`: pull→ack→host restart→cursor survives; un-acked events re-pullable after acked-prefix rotation.
^- [x] Capture modes — `LogCaptureMode` (`full`/`metadata`/`off`, default `full`) on `EngineOptions` (the `metering_listen_port` thread: `engine.rs:207-214` → `:298-336`) + `ServeOptions` (`lib.rs:58-86`) + `--log-capture` on `hekma serve` (`serve.rs:33-54`; parse pins at BOTH binary surfaces — the 17-3 lesson). `full` byte-identical (spawn wiring `supervisor/spawn.rs:542-554` untouched). `metadata`: raw crash-immune redirects continue (see Design Notes); the tailer writes metadata-only `output.log` lines `{ts, stream, attribution, byte_len, sha256}`; raw files unlinked at drain/stop after the final usage pass (best-effort; unlink failure surfaced). `off`: `Stdio::null()` (the `backends/unix/mod.rs:277` precedent), no raw files, no tailer, no `output.log`; capture reads return honest absence (AI-18 — never a fabricated size; `fleet.rs:540` precedent) and start emits a stderr note naming the degraded log-sentinel metering. Tests: full byte-identity pin, metadata at-rest shape + sentinel metering still lands while running, off spawn/read/notice matrix.
^- [x] `agent.log` bound — AD-12 amendment. Mechanism: coordinated TRUNCATE, not rename-generations — the child's stdout fd is inode-pinned to the one direct redirect (`process_backend.rs:55-77`), so renames cannot bound the live file; the raw opens are already O_APPEND (`backends/unix/mod.rs:248-251`), so at the drain/tailer tick, once `agent.log` reaches `LOG_ROTATE_MAX_BYTES` (`process_backend.rs:415`) and BOTH consumers (usage-drain cursor, tailer tail-cursors) sit past the cut, the engine truncates to 0. Consumers' shrink semantics ALREADY exist (`UsageTail::Shrunk`, `supervisor/mod.rs:352-359`; `read_raw_output_since`, `interaction.rs:444`) — extend them honestly and surface each truncation as a notice. Tests: truncate at the cap (cap as a parameter, `should_rotate`-style pure seam), no-double-count preserved, shrink-snap pins.
^- [x] State-dir-on-a-volume — verification + docs only (17-2 shipped the surface): an integration leg asserting lock/db/secrets/outbox all resolve and a second host is refused on an explicit non-default base (`serve.rs:45-54` → `EnginePaths::new`, `paths.rs:354`; `HOST_LOCK_FILE`, `paths.rs:224`); docs subsection "placing the state directory on a Host-chosen volume" in `docs/commands.md:421`, pointed to from `docs/control-api.md` (17-5's container page links it).
^- [x] CI + docs + coverage in the same change — `docs/control-api.md` gains the export/ack contract; `docs/commands.md` the `--log-capture` flag and capture-mode table; README hosting mention updated. No new dependencies on either edge (the engine work is std-only; the host reuses 17-3's axum) — the `cargo tree` diff recorded as EMPTY in the PR. Boundary gate (`ci.yml:658`) and tarpaulin ≥95 (`ci.yml:1246`) green.

**Acceptance Criteria:**
- Given a running engine, `GET /v0/export` hands bounded batches in ONE global order across transitions/breaches/usage; `POST /v0/export/ack` advances a cursor that survives a host restart; un-acked events re-pull; retention is bounded and any eviction of un-acked events is surfaced (`retained_from` + typed refusal), never silent.
- Given the same run, `resync_events` answers byte-identically to 17-3 (wire pin) — the outbox is purely additive.
- Given `--log-capture full` (default), serve/spawn behavior is byte-identical to 17-3; `metadata` leaves no content at rest after stop but real sizes/hashes/timestamps; `off` writes nothing and every surface shows honest absence plus the degraded-metering note.
- Given a long-running agent, `agent.log` never exceeds the 10MB bound and metering stays double-count-free across truncations; given `serve --state-dir` on a Host-chosen volume, lock/db/secrets/outbox live there and a second owner is refused by name (docs state the placement).
- Given handoff: the four house gates pass, coverage ≥ 95%, the (empty) tree diff is recorded in the PR.

</frozen-after-approval>

## Design Notes

- **Global sequence + ack semantics.** The outbox journal is a SECOND durable surface written inside each family's commit path under the registry lock; seq order ≈ commit order (the families keep their own per-family order — the resync contract's honesty notes carry over). `ack_export` stores `max(current, given)` — monotonic, crash-safe via `write_atomically`; one consumer cursor because the host lock (`paths.rs:224`) guarantees exactly one Host owns the state dir. Rotation follows the log-family discipline: the journal is bounded at `LOG_ROTATE_MAX_BYTES × LOG_ROTATE_GENERATIONS` (10MB × 3, `process_backend.rs:415-422`); truncation below the ack cursor only; if the oldest UN-ACKED record is ever evicted, `retained_from` + the below-floor refusal say so — the honest degradation AD-14's "un-acked events survive" is scoped by.
- **What rides 17-3's routes:** only the shape — `?cursor=` query + batch payload + envelope + 409 `cursor_invalid` mapping (`routes.rs:336-363`). The per-family pull route is untouched; the export route is additive v0.x under the same unstable marking.
- **Capture-mode matrix:** `full` = today; `metadata` = content allowed on disk ONLY transiently (the crash-immune redirect requires it), metadata-only at rest — the pipe alternative was rejected because it couples the agent's write-success to engine liveness (the documented review-of-#80 regression, `process_backend.rs:61-68`); `off` = nothing, ever. Metering: `full`/`metadata` keep the sentinel drain (metadata parses before unlink); `off` loses log-sentinel sources — surfaced at start and in fleet/status, never fabricated.
- **CLI-first:** `--log-capture` on `hekma serve` (the mode is a serve/config knob — a whole-engine deployment posture, not per-instance config); export/ack are Control-API surface only (a Host operation, not an operator command) — reachable for tests through the engine facade directly.

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Export pull, no cursor | ack cursor exists / fresh state | resume from ack seq / full backfill, bounded batch | malformed cursor JSON → 400 |
| Export pull, explicit cursor | `{"seq":n}` | events after n + next cursor + `retained_from` | below `retained_from` → 409 naming both counts, never clamped |
| Ack | `{"seq":n}` ≤ committed tail | durable cursor advance (atomic write) | past tail → 409; regressive ack → no-op success |
| Outbox append fails at a commit site | any | family commit STANDS; stderr diagnostic; resync remains the heal | never aborts the operation (graceful degradation) |
| Un-acked eviction | journal cap hit | pull still answers; `retained_from` + refusal name the loss | never silent (AI-18) |
| `--log-capture off` start | serve with mode | stderr note; spawn redirects to null; reads → honest absence | invalid mode value → CLI parse refusal |
| `metadata` stop/drain | running instance | final usage pass, then raw files unlinked; metadata-only `output.log` | unlink failure → surfaced diagnostic |
| `agent.log` at 10MB | consumers past the cut | truncate to 0; cursors snap (Shrunk semantics); notice emitted | consumer lagging → truncation deferred to the next tick |
| `--state-dir` on a volume | serve with explicit base | lock/db/secrets/outbox under base; started notice | second host → refusal naming the live holder |

## Code Map

- `crates/hekma-engine/src/domain/outbox.rs` — NEW (journal, seq, ExportBatch, ack); re-exported beside the resync types (`domain/mod.rs:61`).
- `crates/hekma-engine/src/engine.rs` — `Engine` (:139); `EngineOptions` (:207, port precedent :214); `open_with_options` (:298); `resync_events` (:1439 — the contract mirrored).
- `crates/hekma-engine/src/domain/resync.rs` — `ResyncCursor` (:127); `ResyncBatch` (:166); torn-tail discipline (:63-79); cursor-error contract (:84-92); boundary docs (:96-100).
- `crates/hekma-engine/src/domain/bus.rs:113` — `EngineEvent` (the outbox payload verse).
- `crates/hekma-engine/src/domain/supervisor/mod.rs` — never-rotated note (:335); `UsageTail::Shrunk` (:346-359); transition append (:1304); breach append (:1663).
- `crates/hekma-engine/src/domain/supervisor/usage.rs` — `ingest_usage` (:618); commit-then-publish (:650) — the usage outbox hook.
- `crates/hekma-engine/src/domain/supervisor/spawn.rs:542-554` — the three-path capture wiring the modes branch on; `agent_log_path` (:481).
- `crates/hekma-engine/src/backends/unix/mod.rs` — O_APPEND raw opens (:248-251); `Stdio::null` arm (:277-281).
- `crates/hekma-engine/src/ports/process_backend.rs` — `SpawnSpec::log_file` (:77); `LOG_ROTATE_MAX_BYTES` (:415); `LOG_ROTATE_GENERATIONS` (:422); `should_rotate` (:430 — the pure-seam style the truncate seam copies); pipe-coupling rationale (:61-68).
- `crates/hekma-engine/src/domain/supervisor/interaction.rs:444` — `read_raw_output_since`'s shrink semantics.
- `crates/hekma-engine/src/domain/registry.rs` — `instance_log_path` (:1336); `agent_output_log_path` (:1353); `agent_stderr_log_path` (:1365); `attributed_output_log_path` (:1389).
- `crates/hekma-engine/src/paths.rs` — `write_atomically` (:133); `HOST_LOCK_FILE` (:224); `EnginePaths` (:269); `new` (:354); `state_base` (:368).
- `crates/hekma-host/src/lib.rs` — `ServeOptions` (:58-86); lock refusal (:404-425); engine open (:415+).
- `crates/hekma-host/src/control/routes.rs` — `EventsQuery` (:336); `events_pull` (:347) — the export route lands beside this.
- `crates/hekma/src/cli/serve.rs:33-54` — the flag block `--log-capture` joins.
- `crates/hekma-engine/src/domain/fleet.rs:540` — `METERING_SEED_CELL` (honest-absence precedent).
- `docs/commands.md:421`; `docs/control-api.md`; `.github/workflows/ci.yml` (:658, :1246).

## Verification

**Commands:**
- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --all-targets` — pass, including the extended `control_api.rs` export suite.
- `python3 scripts/check_docs.py` — pass.
- Story-specific: a curl smoke — serve with `--log-capture off` and `--log-capture metadata` against a fake agent; verify the start note, honest-absence reads, and (metadata) the at-rest `output.log` shape after stop; a two-terminal export loop (pull → ack → restart serve → pull resumes at the acked seq); `agent.log` bounded under a chatty fake agent with metering totals unchanged.

## Ask First

- ~~**`metadata`'s transient-on-disk window`**~~ **Ruled by the supervisor 2026-10-19: accept the stated default** — transient-while-running, metadata-only at rest, documented. The pipe alternative is the documented review-of-#80 liveness regression and stays rejected. Guidance added: the docs must ALSO disclose the crash window honestly (an unclean host death in `metadata` mode leaves raw content on disk — that is the cost of crash-immune redirects; state it beside the mode table).

## Spec Change Log

- 2026-10-19 (supervisor approval): approved as drafted — anchors spot-verified; the four ambiguity resolutions are sound (truncate-not-rename for the inode-pinned fd is the physically honest mechanism; the outbox append-failure degradation and retention honesty follow AI-18; state-dir-on-volume correctly scoped to verification+docs). Ask First ruled: accept the metadata default + add the crash-window disclosure. Spec → in-progress.
- 2026-10-20 (review round, 26 findings — all applied): load-bearing — the outbox now refuses engine open on an unreadable journal OR ack file (typed `RegistryError::OutboxOpenRefused` with the quarantine + `resync_events` remediation at every surface) and seeds `max(tail, ack)` so an empty journal under a surviving ack cannot mint below it; a read-time duplicate/decreasing-seq guard makes cross-process double-writes loud (discipline documented in `docs/control-api.md`); compaction can never drop the last entry; interior blank lines are surfaced like tail damage; the append-path ack-read failure is diagnosed, never swallowed; appends are O(1) amortized (in-memory size/cost cache under the supervision lock — no more full-journal reads per commit); the truncate/tailer write-back is race-free (no stale-cursor skip after a snap) with an `agent-stderr.log` truncate twin, a Terminal-drain check, and a once-per-state deferral latch; adopted instances are unlinked by their own per-Run spawn-time capture mode (a Full-mode adoptee's raw logs survive a metadata host, and the refusal message qualifies it); metadata hashes describe RAW bytes before any lossy decode; `limit` parses inside the envelope (400 + `limit must be >= 1`); the `hekma-host` binary gains `--log-capture` parity; a no-op ack skips the durable write; the README claim tightened and `/v0/version` self-describes the capture mode. Verification legs added: the usage + breach outbox hooks driven end-to-end with exact seq interleaving, the off-mode fleet sentinel, the reaper crash-path metadata unlink, and the API-level un-acked re-pull after acked-prefix rotation. RULING (export cursor floor): the frozen text's "below `retained_from` is refused" was imprecise — the implemented and KEPT rule is the lossless one (refuse only when the cursor's window would include evicted entries, i.e. strictly below `retained_from − 1`; `retained_from − 1` itself is servable because nothing in its window was evicted); docs state the precise rule, the frozen block untouched. Baseline moved under us mid-story by the maintainer's stack rework (#310 squash-merged, `hekma-engine` 0.8.0 slot bump, five Windows test-gating commits): `3cc3d61` → `922ae18`, WIP snapshots rebased cleanly. Gates green: fmt / clippy `-D warnings` / workspace tests (known env smoke only) / `check_docs.py` 28 / `test_automation.py` 34/34 / tree gate: no new crates. Spec → done.

## Suggested Review Order

**The journal itself (start here)**

- Append with retention, the tolerant reader (duplicate/decreasing-seq guard), compaction that never drops the last entry, cursor/ack math, and the pure pins.
  [`outbox.rs`](../../crates/hekma-engine/src/domain/outbox.rs)

**The engine wiring**

- Open-time refusal + `max(tail, ack)` seeding; the transition/usage/breach append hooks (O(1) amortized); truncate caps, the stderr twin, the Terminal-drain check, the deferral latch; per-Run capture mode on the adopted-instance unlink path.
  [`supervisor/mod.rs:1157`](../../crates/hekma-engine/src/domain/supervisor/mod.rs#L1157) (`rederive_outbox_seq`), the truncate seams around [`:836`](../../crates/hekma-engine/src/domain/supervisor/mod.rs#L836) · usage/breach hooks in [`usage.rs`](../../crates/hekma-engine/src/domain/supervisor/usage.rs) · capture paths in [`spawn.rs`](../../crates/hekma-engine/src/domain/supervisor/spawn.rs) · crash unlink in [`reaper.rs`](../../crates/hekma-engine/src/domain/supervisor/reaper.rs) · `RegistryError::OutboxOpenRefused` in [`error.rs`](../../crates/hekma-engine/src/domain/error.rs)

**The API surface**

- `GET /v0/export` + `POST /v0/export/ack` against the engine facade; the envelope-internal `limit` parsing; the cursor-floor refusal mapping to `409 cursor_invalid`.
  [`routes.rs:415`](../../crates/hekma-host/src/control/routes.rs#L415)

**The two CLI surfaces**

- `--log-capture` on `hekma serve` and the standalone `hekma-host` binary (one default site, parse pins at both surfaces); the `OutboxOpenRefused` CLI mapper.
  [`serve.rs`](../../crates/hekma/src/cli/serve.rs) · [`main.rs:111`](../../crates/hekma-host/src/main.rs#L111) · [`agent.rs`](../../crates/hekma/src/cli/agent.rs)

**Proof + docs**

- The integration suites: round-trip/restart, honest off/metadata refusals, the usage+breach interleaving leg, the API-level rotation re-pull; the serve flag legs.
  [`outbox_export.rs`](../../crates/hekma-engine/tests/outbox_export.rs) · [`control_api.rs`](../../crates/hekma-host/tests/control_api.rs) · [`serve_cli.rs`](../../crates/hekma/tests/serve_cli.rs) — docs: [`control-api.md`](../../docs/control-api.md) (export contract, cross-process discipline, pagination wedge) · [`commands.md`](../../docs/commands.md) · README's tightened capture-mode claim
