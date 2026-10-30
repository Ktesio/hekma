---
title: 'The wire fence — a `block` Breach Action'
type: 'feature'
created: '2026-10-30'
status: 'in-review'
review_loop_iteration: 1
baseline_commit: 'bdb5d02f7ebd7886efadde3ce610cf9303ef31a6'
context:
  - '_bmad-output/planning-artifacts/epics.md (Story 19.2, amended 2026-10-30 + readiness)'
  - '_bmad-output/implementation-artifacts/epic-19-context.md'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-7 "`block` before the shared proxy"; AD-26 proxy-wire; AD-27 writer)'
  - '_bmad-output/planning-artifacts/implementation-readiness-2026-10-30.md (Story 19-2 findings)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The first production Host enforces money at its own choke point and wants Hekma's cooperative meter to fence as well: today a breach can only `pause`, `stop` or `warn` the agent process, so a Host-run agent keeps calling the model after its ceiling is reached unless the Host kills it. The Host needs a wire-level refusal it can rely on now, built on today's per-instance metering listener, without waiting for the shared proxy (Epic 23).

**Approach:** Add `BreachAction::Block`. When the AD-7 pipeline fires a `block` decision it raises a lock-free per-instance fence on the instance's metering listener; `forward()` then refuses further model calls with a typed, non-retryable response — the first slice of `proxy-wire v1`. The fence is rebuilt from the durable breach record on adoption and engine restart, evaluated at Run start so a cumulative ceiling cannot be escaped by a restart, refused at configuration where no listener exists, and published additively in the governance vectors.

## Tasks & Acceptance

**Execution:**
- [x] `domain/budget.rs` — `BreachAction::Block` (wire `"block"`), never `#[default]`; `FromStr`/`as_str`/the parse error's accepted set gain it. No `#[non_exhaustive]` (keeps cross-crate exhaustive matches as the compile-time catch; recorded below).
- [x] `domain/governance.rs` — the fence's pure decision consumer: `fence_refusal(reasons) -> Option<FenceRefusal>` (the first reason in canonical order: tokens before dollars, per-run before cumulative) plus the refusal constants (status 402, type `hekma_budget_block`, the four codes). One implementation, shared by the listener and the vectors.
- [x] `metering/` — `WireFence`: an `Arc<AtomicU8>` reason bitmask (one bit per `(dimension, scope)`), `raise` (Release) and `refusal` (Acquire load → `governance::fence_refusal`). `ObservedListener` owns one, exposes `fence()`, and passes it to `forward()`. `forward()` checks it FIRST: a fenced, non-safe-method request (anything but GET/HEAD/OPTIONS) is answered with the refusal and is never forwarded, metered or body-buffered. No lock in the request path (AD-18).
- [x] `supervisor/usage.rs` — `apply_breach`'s `Block` arm raises the fence on the instance's listener after the breach is recorded (record-first-then-act unchanged) and emits the fence breadcrumb on the engine diagnostic surface; an instance with no listener records the breach, acts as `warn`, and emits a loud diagnostic naming the reason (the source changed after the write, a hand-edited layer, or an adopted instance whose listener could not be re-bound).
- [x] Run start (`spawn.rs` + a pipeline helper in `usage.rs`) — after the listener starts and before the process spawns, evaluate `decide()` against a fresh Run (per-run totals zero, cumulative totals from the ledger, empty latch) and raise the fence for every `block` decision; after the `Supervised` insert, when the resolved action is `block`, run `enforce_budget` once so those breaches are recorded and latched under the new Run. Other actions keep today's first-usage-event semantics, byte-unchanged.
- [x] Adoption (`reaper.rs`) — rebuild the fence on the re-bound listener from `breaches.log`: events for the resumed run id whose action is `block` and equals the currently configured action (the existing latch action filter). An adopted instance without a re-bound listener gets no fence and the existing strand diagnostic.
- [x] Restart Policy relaunch — a relaunch begins a new Run in today's code, so a per-run fence lifts with it like the per-run latch does; when the previous Run carried a per-run `block` breach, emit a diagnostic saying so and pointing at a cumulative ceiling (deferred-work entry, trigger: story 21.2's Run carries relaunches).
- [x] Config (`registry.rs` `set_config`) — refuse `budget.breach_action = block` with `ConfigError::InvalidValue` when the instance's resolved Metering Source is not `engine-observed` (acp kinds resolve from effective config, others from the adapter snapshot), naming the source and the remediation; an unreadable source refuses too (fail closed).
- [x] Surfaces — hekma-host OTLP `action_str` gains `block`; the Fleet/CLI budget cell prints it through `as_str`; governance vectors 1.0.0 → 1.1.0 (new cover tag `block-fence`, block decision cases, new `fence` case kind, regenerated checksum); CI stand-in Host + `scripts/test_automation.py` learn the ninth tag and the third kind.
- [x] Release surface (AI-55) — hekma-engine 0.9.0 → 0.10.0 (new variant on an exhaustive public enum = breaking for 0.x); CHANGELOG + RELEASE_NOTES announce it; the post-merge semver baseline bump (ci.yml + `scripts/test_automation.py`, to the squash SHA) is recorded as the follow-up step, as PRs #181/#184/#193 did.
- [x] Docs — `docs/` budget/metering pages: `block`, the refusal contract (proxy-wire v1 first slice), when the fence lifts, the config refusal, and the cooperative scope (NFR-6); `docs/governance.md` for the new tag/kind and 1.1.0.

**Acceptance Criteria:**
- Given an engine-observed instance with `budget.breach_action = block` and a ceiling, when a committed usage event reaches it, then the breach event records `action: "block"`, the engine diagnostic names the fence, and the next POST through the listener gets HTTP 402 with the contract body and never reaches the upstream.
- Given a fenced listener, when the agent sends GET `/v1/models`, then it is forwarded (not fenced).
- Given a fenced instance, when the engine restarts and adopts it on the same pinned port, then the fence is up again before any new usage arrives, and no duplicate breach line is written.
- Given cumulative usage already at a `block` cumulative ceiling, when the instance starts a new Run, then the very first model call is refused, and the breach is recorded under the new run id.
- Given a `self-reported` instance, when `hekma agent config set <name> budget.breach_action block` runs, then it fails with a diagnostic naming the source and the config file is byte-unchanged; `pause`/`stop`/`warn` still succeed.
- Given `block` resolved on an instance with no listener, when it breaches, then the breach is recorded and a diagnostic says the fence could not be raised and the breach acted as `warn`.
- Given no `budget.breach_action` anywhere, the resolved action is never `block`.
- Given the governance vectors, they verify byte-identically at `vectors_version` 1.1.0, every pre-existing case is unchanged, and the nine tags are covered.
- Given handoff: fmt, clippy `-D warnings`, the workspace tests and `check_docs.py` pass; coverage ≥ 95%; AI-55 two-pass review done.

</frozen-after-approval>

## Design Notes

- **The refusal response — `proxy-wire v1`, first slice.** Status **402 Payment Required**. Headers: `content-type: application/json`, `x-should-retry: false` (Stainless-generated SDKs — OpenAI and Anthropic, Python and TypeScript — honour it ahead of the status), no `retry-after`. Body, compact JSON in this field order:
  `{"type":"error","error":{"type":"hekma_budget_block","code":"tokens_per_run","message":"…","param":null,"dimension":"tokens","scope":"per-run","proxy_wire":"v1"}}`.
  `error.type` is the stable class; `error.code` ∈ {`tokens_per_run`, `tokens_cumulative`, `dollars_per_run`, `dollars_cumulative`} names dimension and scope (snake_case, the OpenAI code convention); `dimension`/`scope` repeat them in the house wire spelling. `message` is a fixed per-code sentence; no limit, observed total, instance name, request or response content. The envelope parses as an OpenAI error (`error.message/type/code/param`) and an Anthropic error (top-level `type: "error"`, `error.type/message`). Rejected: 429 and 5xx/408/409 (retried by the OpenAI, Anthropic and Vercel AI SDKs); 403 (reads as a credential failure — some agents re-prompt for auth); 400 (reads as a malformed request). 402 has precedent as a non-retryable "out of credit" signal in OpenAI-compatible gateways. The breach response header the spine names for passing calls is NOT emitted here — 23.1 names and documents it. 23.1 documents this response unchanged; 23.4 reuses it for `reject`.
- **When the fence lifts.** The fence lives on the listener, which lives for one Run, so it lifts when the Run ends (operator stop → start, or a Restart Policy relaunch, which begins a new Run in today's code). "Raised" takes effect at the next Run start: the Run-start evaluation re-checks the cumulative ceiling, so raising it and starting a new Run un-fences, and leaving it lets the first call of the new Run be refused. A live un-fence on a raised ceiling needs a config-write trigger; it lands with 21.6's within-Run latch re-arm (readiness finding, refuted to low; option 1 taken). Changing the action away from `block` mid-Run does not lift a raised fence.
- **Fence = reasons, not a boolean.** One `budget.breach_action` key governs both dimensions and both scopes, so the fence carries each `(dimension, scope)` reason that fired `block`; the refusal names the first in canonical order. Every reason clears only with the Run.
- **Safe methods pass.** GET/HEAD/OPTIONS never carry a billable completion (their responses are still skimmed for `usage` like any other); letting them through keeps a fenced agent's model listing and probes alive so it hits the refusal on its next model call instead of crash-looping at startup.
- **Run-start evaluation is the pipeline.** The pre-spawn fence raise uses `governance::decide` (the same core) over a fresh-Run input, in the enforcement module; the post-insert `enforce_budget` records and latches. Gating both on `block` keeps `pause`/`stop`/`warn` byte-unchanged. AD-7's "only the pipeline triggers a Breach Action" holds.
- **Scent-Check Mode.** None exists yet; `forward()` has no bypass of any kind, so `block` refuses whenever configured. 22.3 must not add one (the amended FR-43).
- **Never a default.** `#[default]` stays `Pause`; the engine-default layer carries no `budget.breach_action`. Pinned by a test.
- **No `#[non_exhaustive]`.** It would force `_` arms in hekma-host and hekma-conformance and remove the compile-time catch that found this story's OTLP mapping; the variant is announced as a 0.10.0 break instead.
- **After 21.1.** The store writer sets the flag (AD-27); this story's raise point (`apply_breach`) is the seam 21.1 moves.
- **Born fenced.** The Run-start and adoption reasons are raised on a `WireFence` handed into the listener's start, so the accept loop never serves a request before the fence is up.
- **`block` set mid-Run.** The latch is keyed by `(dimension, scope)`, not action, so a breach recorded under `warn` never fires again. When the action becomes `block`, the next enforcement pass finds pairs that breach under `block` but are latched, and raises the fence with a diagnostic and no second breach record.
- **Refused bodies are drained, not buffered** (up to the relay cap), so a keep-alive client still uploading reads the 402 instead of a reset its SDK would retry. An `Expect: 100-continue` request is answered at once, so its body is never invited.
- **Cooperative scope (NFR-6).** The fence refuses calls routed through Hekma's listener. An agent that calls the provider directly, or ignores `base_url`, is not fenced; the Host's own choke point remains the enforcement of record.

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|---|---|---|---|
| Breach with `block` | observed usage reaches a ceiling | breach event `action: block`; fence up; diagnostic | append failure surfaced (existing) |
| Fenced POST | any non-safe method | 402 + contract body; not forwarded, not metered | — |
| Fenced GET | `/v1/models` | forwarded faithfully | upstream errors stay honest 502s |
| Adoption on the pinned port | breaches.log has a `block` event for the run | fence rebuilt before new usage | unreadable log: existing diagnostic, fence not rebuilt |
| New Run over a cumulative `block` ceiling | cumulative ≥ ceiling | fence up before spawn; breach recorded under the new run | config read failure: no fence, same as enforcement |
| Config write `block` | self-reported instance | `InvalidValue` naming the source | unreadable snapshot → refused |
| `block` with no listener | hand-edited layer / source change / strand | breach recorded, acts as `warn`, diagnostic | — |
| Restart Policy relaunch after a per-run fence | previous run had a per-run `block` breach | new Run unfenced; diagnostic | — |

## Code Map

- `crates/hekma-engine/src/domain/budget.rs` — `BreachAction` (:86) + `FromStr` (:139).
- `crates/hekma-engine/src/domain/governance.rs` — `fence_refusal`, `FenceRefusal`, refusal constants, `FenceCase` kind, `CoverTag::BlockFence`, block decision cases, version 1.1.0.
- `crates/hekma-engine/governance/vectors-v1.json{,.sha256}` — regenerated through the bless lever.
- `crates/hekma-engine/src/metering/listener.rs` — `WireFence`, `ObservedListener::fence`, `forward()` refusal (:521), `refusal_response`.
- `crates/hekma-engine/src/domain/supervisor/usage.rs` — `apply_breach` (:1140) `Block` arm, `enforce_block`, Run-start helper, shared arm builder.
- `crates/hekma-engine/src/domain/supervisor/spawn.rs` — pre-spawn raise after `start_observed_listener` (:356); post-insert `enforce_budget` (:1192); relaunch diagnostic.
- `crates/hekma-engine/src/domain/supervisor/reaper.rs` — adoption fence rebuild beside the latch seed (:1659).
- `crates/hekma-engine/src/domain/registry.rs` — `set_config` (:1135) refusal; resolved-metering-source helper.
- `crates/hekma-host/src/otlp/payload.rs` — `action_str` (:582).
- `.github/workflows/ci.yml` (governance-vectors stand-in Host), `scripts/test_automation.py`.
- `Cargo.toml` (workspace dep 0.10.0), `crates/hekma-engine/Cargo.toml`, `CHANGELOG.md`, release notes, `docs/`.

## Verification

**Commands:**
- `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets`; `python3 scripts/check_docs.py`; `python3 scripts/test_automation.py`.
- `cargo test -p hekma-engine governance` — vectors verifier green at 1.1.0.
- `sha256sum -c crates/hekma-engine/governance/vectors-v1.json.sha256`.
- Coverage: `cargo tarpaulin --workspace --fail-under 95` (or the CI coverage job).

## Ask First

None blocking. Decisions taken under the delegated brief and recorded above for Islam's review: status 402 (vs 403), the safe-method pass-through, the fence lifting with the Run (live un-fence deferred to 21.6), and the Restart Policy relaunch lifting a per-run fence until 21.2.

## Spec Change Log

- 2026-10-30 (draft): authored against `bdb5d02` (PR #353, the readiness gate, which carries the amended 19.2, the AD-7 bullet and the refreshed epic-19 context). Headless: delegated by a parent session.
- 2026-10-30 (implementation): every Execution item landed; the frozen block is untouched. Notes against its letter: the vectors 1.1.0 policy text in docs/governance.md was rewritten to say what a minor may add (new kinds and values, appended after every shipped case; a pinned consumer re-runs its version's leading prefix) — the old wording called any schema change major; the 1.0.0 artifact never shipped in a tagged release. The CLI `governance-vectors` summary now counts the `fence` kind (the full-suite run caught it folding fences into derivations).
- 2026-10-30 (review round 1, AI-55): primary pass by the implementer plus an independent adversarial pass. Applied: (1) the adoption fence was raised after the re-bound listener started serving — the fence is now handed into the listener start (`start_on_port_with_fence`), Run start likewise; (2) switching to `block` after a breach latched under another action raised nothing — `enforce_budget` now fences latched pairs that breach under `block` (`fence_late_block`); (4) the Run-start fence and its record used two config reads — the post-insert enforcement now runs whenever the Run-start fence raised; (5) a direct `run_start_fence` test; (6) "safe methods are never metered" was false — docs corrected; (7) refused bodies are drained, and `Expect: 100-continue` skips the drain (mutation-checked: without the drain a 2 MiB fenced upload reads a reset); (9) stale `pause/stop/warn` docstrings, a missing period in docs/architecture.md, and a `scripts/test_automation.py` pin that the CI stand-in Host requires every case kind. Deferred: (8) the fence on the Fleet/status surface (deferred-work, trigger AD-26 dispositions). Already covered: (3) by the policy rewrite above; (10) by the relaunch diagnostic, its deferred-work entry and the hosting docs.
