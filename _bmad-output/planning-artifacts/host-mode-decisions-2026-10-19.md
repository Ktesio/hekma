---
type: architecture-decision-proposal
date: 2026-10-19
author: Winston (System Architect) via the BMAD build of epic-17 story 17.1, for Islam
status: ratified-2026-10-19  # AI-58 draft-then-ratify: ZERO planning artifacts were touched to produce this. Ratified by Islam 2026-10-19 — D1 hekma-host / D2 axum 0.8 / D3 re-ratify coarse (all recommendations as proposed); §4's one-change application landed in the same change.
decides: "The three decisions AD-20 deferred to the host-mode spec stage — D1: the host crate's name; D2: the Control API's HTTP stack (NFR-8 review); D3: the AD-18 lock-model outcome (reopen trigger (1), fired by AD-20's ratification)."
gate: "AD-20: 'Settled at 15.1's spec stage' — realized as this spec stage. Nothing here edits ARCHITECTURE-SPINE.md, the PRD, epics.md, or the sprint tracker; §4 lists the exact edits ratification applies."
method: "D3 decided BY MEASUREMENT — the perf-budgets harness's new --serve-shape scenario drove the engine's PUBLIC async facade under serve-shaped concurrency on 2026-10-19 and the fixed decision rule was applied to the numbers. D1/D2 decided by bounded options with stated trade-offs, exactly one recommendation each."
verified_against_code: true
measured_at_commit: "this worktree, branch feat/epic-17-1-host-mode-spec (uncommitted at measurement time; the harness commit carries the run's docs)"
inputDocuments:
  - _bmad-output/implementation-artifacts/spec-17-1-host-mode-spec-the-shim-crate-and-the-lock-model.md
  - _bmad-output/planning-artifacts/ai-63b-lock-model-decision-2026-08-27.md
  - _bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-2, AD-13, AD-17, AD-18, AD-20)
  - crates/hekma-engine/examples/perf-budgets.rs (the --serve-shape scenario)
  - docs/testing.md (the serve-shape section + the measured serve record)
---

# Host-Mode Decisions — the shim crate's name, its HTTP stack, and the lock model

**Winston (System Architect) · 2026-10-19 · for Islam**

AD-20 ratified the Service Host as a shim over the public engine API and deferred exactly three
decisions to this spec stage. D3 (the lock model) was routed to MEASUREMENT by AD-18's reopen
trigger (1) — the serve shape below measured it. D1/D2 are bounded options. Each decision carries
exactly one recommendation; nothing is applied to the spine until you pick (AI-58).

---

## 0. The measured evidence (D3's input)

The perf-budgets harness (`crates/hekma-engine/examples/perf-budgets.rs`) gained a `--serve-shape`
scenario (story 17.1's deliverable): on the NFR-4 fixture (25 registered / 10 running heartbeat
idlers) PLUS one pathological instance, it drives the engine's PUBLIC async facade for one 12 s
window from the driver's own small multi-thread tokio runtime — the docs/embedding.md host shape,
no HTTP transport, no private reach-in:

- **4 concurrent reader tasks** round-robin all 26 names through the same four content-asserted
  read kinds the default phase measures (`fleet()`, `instance_status()`, `effective_config()`,
  `usage_via_fleet`). The load model is DERIVED, not chosen: each reader carries the default
  phase's OWN read demand (200 iterations spread across the window — 60 ms pace), so per reader
  the demand EQUALS the default phase's and the four concurrent readers deliver ~4× the default
  phase's AGGREGATE (~800 iterations/window) — a serve shape, not a single-pass replay. The pace
  is sleep-only think-time, so a reader stalled on the bounded stop delivers fewer than its
  nominal 200 (the report records the delivered per-reader count beside the nominal demand). The
  process-wide CPU sampling cannot separate the host's handling cost from supervision, and the
  load model must not be a free knob next to a gate.
- **1 subscribed follower** rides the lock-free `Engine::subscribe()` bus counting committed
  events (`--follow`-style); a `Lagged` recovery would be counted and surfaced, never a crash.
  Its count is a handful of lifecycle events in the window (the pathological's start/stop
  transitions and any committed usage/breach events) — bus LIVENESS under load, not load
  evidence.
- **1 pathological instance** is stopped with a bounded 2 s window at t+3 s, mid-load — the
  contention term. The fixture is genuinely slow to stop on each OS: Unix uses the lifecycle
  suite's proven `sh -c 'trap "" TERM; sleep 120'` shape (the agent ignores SIGTERM, so the
  supervisor mutex is held for the whole graceful window before the SIGKILL escalation; the
  self-exit sleep is derived from the serve orphan bound, so the fixture can never self-exit
  inside the schedule it is validated against); Windows uses the lingering `fake_agent` (the
  graceful step always elapses the window before `TerminateJobObject`). Both run through the
  same conformance `ManifestFixture` machinery.
- **CPU/RSS** are sampled on a separate thread with the same sysinfo discipline WHILE the load
  runs; reads feed one merged per-kind distribution. The sampler window is clamped to the serve
  load window, and per-instance figures normalize over the schedule-aware effective Running
  count (the full count until the bounded stop completes, one fewer after — the conservative
  divisor, disclosed as `effective_running_instances` in the report).

**The run of record** (linux x86_64, Intel i7-7820HQ, 8 logical cores, release profile, local
strict gating, 2026-10-19, the review-hardened harness; 11 instances Running, per-instance
figures normalized over the schedule-aware EFFECTIVE count of 10 — the conservative post-stop
divisor; full figures in docs/testing.md's serve record and the run's `serve_shape` report
block):

| Serve gate (own names; bind only on serve runs) | Measured | Budget | Verdict |
|---|---|---|---|
| `serve_read_p99_lt_1s` | **5.07 ms** overall p99 (610 samples/kind) | strict < 1000 ms | PASS (~197× headroom) |
| `serve_cpu_per_instance_le_2pct` | **1.94 %**/instance (19.39 % aggregate) | ≤ 2 %/instance (strict locally) | PASS (~1.03× headroom — the tight one) |
| `serve_rss_per_instance_le_50mib` | **0.96 MiB**/instance | ≤ 50 MiB/instance | PASS (~52× headroom) |

**The bounded stop, disclosed:** elapsed **2021 ms** (window 2 s), final state `stopped`, last
cause `{"kind":"stop-forced","detail":"graceful window (2s) elapsed; escalated to a forced kill
of the process group/job"}` — recorded verbatim. The contention proof is IN the record: **4
reads started inside the stop's measured window** (zero would have failed the run with a named
exit-2 before any gate verdict). Its cost lands honestly in the distributions: exactly the reads
in flight during the window stalled ~2.0 s (fleet max 1997.0 ms — ~4 of 610 fleet samples,
0.7 %), while p99 stayed at 5.07 ms. A coarse-mutex hold is visible, bounded, and amortizes; it
does not cascade. The follower counted 4 events with 0 `Lagged` recoveries — bus LIVENESS under
load, not load evidence (a handful of lifecycle events in the window). Delivered iterations per
reader: 153/152/153/152 of the nominal 200 — the pace is sleep-only think-time, so the stall
costs iterations, recorded beside the nominal demand.

**Measurement provenance (recorded honestly):** two earlier development runs at a HEAVIER,
freely-chosen load model (an unpaced spin, then a 10 ms pace — 4-6× the shipped demand) measured
CPU at 7.61 and 6.55 %/instance respectively, over the same budgets — the process-wide figure
scales with the driver's own call volume, which is why the shipped load model is anchored to the
harness's ratified read-demand premise instead of picked next to a gate. Those runs are
superseded observations, not the evidence. A third same-day run under the pre-hardening harness
(identical load model, but normalizing over the pre-stop count of 11 and without the contention
proof) recorded 5.85 ms / 1.74 % / 0.86 MiB; it is superseded by the run of record above — not
comparable (the divisor changed), same verdict.

---

## D1 — The host crate's name

AD-20 binds the crate to "depend only on `hekma-engine`'s public API — one AD-2 boundary-gate
entry". The name enters the workspace family (`hekma-*`), the AD-2 publishable-crate list, the
CI boundary-gate allowlist, and the crates.io namespace.

### Options

**Option A — `hekma-host`** (RECOMMENDED)
- *Trade-offs for:* the family's naming is uniform (`hekma-engine`, `hekma-adapter-api`,
  `hekma-adapters-hermes`, `hekma-conformance`, `hekma`) and AD-20's own vocabulary is "the
  Service **Host**" — the crate name is the AD, verbatim. Short, pronounceable, and it does not
  overweight the transport (the host is a shim that HAPPENS to serve HTTP; D2 is replaceable,
  the host role is not). Boundary-gate regex stays one line.
- *Trade-offs against:* "host" alone is generic on crates.io (mitigated: the `hekma-` prefix is
  the namespace, and the crate is workspace-published alongside the family).

**Option B — `hekma-server`**
- *Trade-offs for:* describes what it does at a glance (serves HTTP).
- *Trade-offs against:* names the IMPLEMENTATION detail, not the role; if D2 ever swapped
  transports, the name would lie. AD-20 deliberately says "Service Host", not "server".

**Option C — `hekma-control-plane`**
- *Trade-offs for:* the served surface IS a Control API.
- *Trade-offs against:* fights the PRD's own vocabulary — PRD §8 rules OUT a hosted control
  plane; a crate named control-plane invites exactly that misreading. Rejected on naming alone.

**✅ Recommendation D1: `hekma-host`.** The AD-20 phrase, the family pattern, and the
transport-agnostic role all point the same way.

### Drafted ratification wording (NOT applied — §4 applies it)

- AD-2 crate list gains: "`hekma-host` (bin + lib; the AD-20 Service Host — depends only on
  `hekma-engine`'s public API plus its own Control-API types)".
- The CI boundary-gate allowlist line
  (`grep -Evx 'hekma-(engine|adapter-api|adapters-hermes)'`, ci.yml) becomes
  `hekma-(engine|adapter-api|adapters-hermes|host)` — and `scripts/test_automation.py`'s pin of
  that exact string (test_ci_... asserts `hekma-(engine|adapter-api|adapters-hermes)`) MUST be
  updated in the SAME change, or the boundary job's own pin reds.
- AD-20's deferred note ("Settled at … spec stage") is replaced by the outcome: name, stack, and
  the AD-18 verdict pointer.

---

## D2 — The Control API's HTTP stack (NFR-8 review)

AD-20 requires: a LOCAL HTTP/JSON Control API, live events over SSE + pull on a global cursor,
bearer auth resolved through `SecretResolver`, explicit bind, bounded SIGTERM drain. NFR-8
(runtime-lean dependencies) governs the footprint. Relevant tree fact: **hyper 1.12, http 1.4,
http-body-util, and tokio are ALREADY runtime dependencies of `hekma-engine`** (the observed-
metering loopback forward listener) — any stack choice reuses them.

### Options

**Option A — axum 0.8** (RECOMMENDED)
- *NFR-8 footprint:* adds `axum` + `axum-core` + the tower family (`tower`,
  `tower-layer`, `tower-service`, `sync_wrapper`, `matchit`) on top of the hyper/http/tokio
  stack already in-tree. No TLS dependency (a local bind; TLS is a deployment concern, never a
  crate feature). Modest, auditable, single-family.
- *SSE:* first-class (`axum::response::sse::Sse` with keep-alive) — AD-20's live-events
  requirement is a constructor call, not a hand-rolled chunked writer.
- *Fit:* extractor + router model maps 1:1 onto the Control API's JSON bodies and bearer
  middleware (one tower layer over `SecretResolver`); graceful drain composes with tokio's
  shutdown select; `0.x`-versioned Control API matches axum's own explicit-versioning posture.
- *Maintenance risk:* lowest of the three — the tokio ecosystem's maintained default; semver
  discipline is proven (0.7→0.8 migrations were bounded and documented).
- *Trade-offs against:* it IS a framework — more surface than the task strictly needs; tower's
  concepts (layers/services) have a learning curve for contributors.

**Option B — hyper 1.x direct (router + SSE hand-rolled over `http-body-util`)**
- *NFR-8 footprint:* effectively ZERO new crates (hyper/http/http-body-util already in-tree).
  The strongest possible NFR-8 story.
- *SSE:* hand-rolled — chunked transfer framing, keep-alive comments, and the drain path are
  bespoke code with real correctness edge cases (a mis-framed SSE stream is a hung follower).
- *Fit:* every route, body parse, auth check, and the 404/405 matrix become hand-maintained
  surface — precisely the "second business-logic implementation growing inside a transport
  layer" AD-20 prevents, one layer down.
- *Maintenance risk:* highest of the three — we own HTTP semantics forever; upstream fixes to
  framing/DoS behavior never reach us.

**Option C — minimal std TCP + hand-rolled HTTP/1.1**
- *NFR-8 footprint:* near-zero deps, but re-implements parsing, chunking, keep-alive, and
  concurrency shaping — HTTP parsing bugs are security surface, and NFR-8 is *lean*, not
  *zero-at-any-cost*. The engine already standardizes on tokio; a second I/O story inside one
  binary is its own smell.
- *Maintenance risk:* extreme; rejected.

**✅ Recommendation D2: axum 0.8.** The marginal crates ride the stack already in-tree; SSE,
auth-middleware, and drain are first-class instead of hand-rolled; the maintenance risk is the
ecosystem's most-buffered. Hyper-direct stays the honest fallback if the NFR-8 review of the
actual `axum` tree at implementation time (17.2's first step: `cargo tree` diff recorded in the
PR) shows the tower family has grown beyond this estimate.

---

## D3 — AD-18 verdict: the lock model under the serve shape

**The rule (fixed at spec time, applied mechanically):** contention over ANY of the three NFR-4
budgets in the serve shape → recommend activating the archived per-instance plan
(`ai-63b-lock-model-decision-2026-08-27.md`); all budgets hold → recommend re-ratifying the
coarse model, citing the measured headroom. The proposal never argues past the numbers.

**Applied to the run of record (§0):**

- `serve_read_p99_lt_1s`: 5.07 ms vs < 1000 ms — holds, ~197× headroom.
- `serve_cpu_per_instance_le_2pct`: 1.94 % vs ≤ 2 % — holds, ~1.03× headroom (the tight figure;
  it is process-wide, includes the host's own handling cost at the documented load model, and is
  now normalized over the CONSERVATIVE post-stop count of 10 — at 4-6× that demand the SAME
  shape measured over budget, recorded in §0's provenance note).
- `serve_rss_per_instance_le_50mib`: 0.96 MiB vs ≤ 50 MiB — holds, ~52× headroom.

**✅ Recommendation D3: RE-RATIFY the coarse two-mutex lock model (AD-18 stands unamended),
citing the measured serve-shape evidence above.** The archived per-instance plan stays archived
— designed, not disproven — and AD-18's existing reopen triggers remain the standing guard. The
honest disclosures that belong in the ratification record: (1) the worst observed stall is the
bounded stop itself, ~2.0 s on in-flight reads (4 fleet reads started inside the stop's window
and stalled — ~4 of 610 fleet samples, 0.7 %; p99 5.07 ms) — a coarse mutex hold is real,
bounded, and amortizes; (2) the CPU headroom
is the shape's tight budget at ~1.03× under the conservative divisor, and it is
load-model-sensitive — any future host demand growth re-runs `--serve-shape` (cheap, wired into
CI as a blocking step) before scaling claims are made.

If instead ANY budget had failed, the recommendation below would have been: activate the
archived plan's Steps 0-4 as the next epic — that arm was not taken; the numbers did not
require it.

---

## 4. What ratification applies (the ONE-change rule)

After Islam picks (or amends), ONE change applies the picks verbatim:

1. **ARCHITECTURE-SPINE.md** — AD-2 crate list + the drafted boundary-gate wording (D1);
   AD-20's "Settled at … spec stage" note replaced by the three outcomes with this doc as
   provenance; AD-18 gains the re-ratification note citing the measured serve-shape run (D3).
2. **`.github/workflows/ci.yml`** — the boundary-gate allowlist gains `|host` (D1, only if
   `hekma-host` is picked).
3. **`scripts/test_automation.py`** — the pin of the allowlist string updated to match (same
   change, or the boundary job's pin reds).
4. **`docs/`** — the host crate named in the shipping docs where AD-20's host is described.
5. **Sprint tracker** — the ratification-gated task ticked; sprint-status updated.

No spine/PRD/epics/tracker byte was touched to produce this document (verified: this worktree's
diff contains only the harness, CI, docs/testing.md, the automation pins, and this file).

## 5. Ratification record (2026-10-19)

Islam picked all three recommendations as proposed: **D1 `hekma-host` · D2 axum 0.8 · D3
re-ratify coarse.** Applied verbatim in one change to: ARCHITECTURE-SPINE.md (AD-2 crate-list
amendment, AD-20 settled note → outcome, AD-18 trigger-(1) closure note, *Amendments
(2026-10-19)* block), `.github/workflows/ci.yml` (boundary allowlist `|host`),
`scripts/test_automation.py` (the allowlist pin), `docs/testing.md` (the host crate named), and
the sprint tracker (17-1's ratification-gated task ticked). The AD-18 per-instance plan stays
archived; the standing reopen triggers remain the guard.
