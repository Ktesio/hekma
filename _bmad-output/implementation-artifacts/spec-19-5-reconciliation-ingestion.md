---
title: 'Reconciliation ingestion — the `reconciled` label''s first producer'
type: 'feature'
created: '2026-11-14'
status: 'in-progress'
review_loop_iteration: 0
baseline_commit: '8f6e91c'
context:
  - '_bmad-output/planning-artifacts/epics.md (Story 19.5; Epic 19 order: after 19-4)'
  - '_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md (FR-45; FR-23 labels; FR-19 open question 8: a gateway may feed Hekma through reconciliation)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-8 and its 2026-09-30 and 2026-10-23 amendments: reconciliation is the reconciled label''s first producer, never an overwrite, a new series in telemetry; AD-14 events; AD-20 authority; AD-27 one writer)'
  - 'crates/hekma-engine/src/domain/cost.rs (EstimateLabel::Reconciled, the forward seam no code path produced until now)'
  - 'crates/hekma-engine/src/domain/runs/{mod,ops}.rs (RunView, RunEventKind, the Run rollup a figure is compared against)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Every figure Hekma shows is `estimated`: tokens counted at the proxy or reported by the agent, dollars derived from a configured Rate. A Host often holds the provider's own invoice or a gateway's confirmed count for a Run, and has no way to put it beside Hekma's figure. `EstimateLabel::Reconciled` exists as a forward seam with no producer, and FR-45 asks for one.

**Approach:** A Host or the Operator submits confirmed usage for a Run (a supervised Run or any child Run): input, output and cached tokens, and optionally confirmed dollars, with a source and an idempotency reference. Each submission is appended to a new `run_reconciliations` table; nothing estimated is touched. The newest submission is the Run's current reconciled figure, and its drift against the estimated figure (both cover the Run and its descendants) is computed on read. `hekma run show`, `GET /v0/runs/{id}` and the Run document carry a `reconciled` block, label every figure `estimated` or `reconciled`, and report the drift. One `reconciled` Run event records each accepted submission. Budgets keep enforcing on estimated figures.

## Tasks & Acceptance

**Execution:**
- [ ] `store/reconcile_sqlite.rs` — schema v26: `run_reconciliations (id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, reference TEXT NOT NULL, source TEXT NOT NULL CHECK (source IN ('provider','host')), input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, cached_tokens INTEGER, cost_micros INTEGER, note TEXT, submitted_by TEXT NOT NULL, submitted_at TEXT NOT NULL, UNIQUE (run_id, reference))`, an index on `(run_id, id)`; insert-or-replay and the newest/all reads.
- [ ] `domain/runs/reconcile.rs` — `Reconciliation` (the submission), `ReconciledView` (the current figure, its drift against the estimated subtree figure, the count of submissions), validation (non-negative counts within i64, a reference of 1–128 printable characters, a note of at most 512, `source` `provider` or `host`), and the writer job: refuses an unknown Run; a replay of `(run, reference)` with the same figures is a no-op returning the stored row, and with different figures is refused (`reconciliation-conflict`); an accepted one commits one `reconciled` Run event.
- [ ] `Engine::reconcile_run` and `Engine::run_reconciliations` (+ blocking twins); `RunView.reconciled: Option<ReconciledView>`.
- [ ] `hekma run reconcile <run-id> --input N --output N [--cached N] [--dollars D] [--source provider|host] [--ref ID] [--note TEXT] [--json]` (routed through the Service Host); `hekma run show` prints the RECONCILED line with drift.
- [ ] `POST /v0/runs/{id}/reconciliation` (Operator or Host bearer; a Run Key is refused) and `GET /v0/runs/{id}/reconciliation` (every submission, newest first).
- [ ] Docs (runs, commands, Control API, events), CHANGELOG + RELEASE_NOTES banners, the site port, sprint status.

**Acceptance Criteria:**
- Given a Run with estimated usage, when a Host submits confirmed figures, then `run_reconciliations` holds the submission, the estimated rollup and ledger are byte-identical, and `hekma run show` shows both figures, each labelled, with the drift (reconciled − estimated) per dimension.
- Given the same `(run, reference)` resubmitted with identical figures, then nothing new is stored and the answer is the stored submission; with different figures, `409 reconciliation-conflict`.
- Given a child Run, then a submission for it is accepted and shown on it; its parent's figures are unchanged.
- Given an unknown Run `404 run-not-found`; a negative or non-numeric figure `400`/`422`; a Run Key on the POST `403`.
- Given a budget, then breaches still evaluate estimated usage only.
- Given the Run event stream, then each accepted submission is one `reconciled` event; a replay emits none.

</frozen-after-approval>

## Design Notes

- **A submission states the Run's confirmed totals, not a delta.** Providers and gateways report totals for a period; a total is idempotent by nature and comparable with Hekma's subtree figure. The newest is current; earlier ones stay readable (`GET …/reconciliation`), never edited.
- **Like for like.** The estimated figure a submission is compared with is the Run's rollup, which covers its descendants, so a Host reconciling a parent submits the parent's whole subtree.
- **Dollars are optional.** A confirmed count with no confirmed cost shows the token drift and no dollar drift; Hekma never derives a `reconciled` dollar figure from its own Rate (that would be an estimate wearing the wrong label).
- **No enforcement change.** Budgets keep acting on what the proxy and agents report as it happens; reconciled figures arrive later and would make enforcement retroactive.
- **Authority (AD-20).** The Operator and a Host may reconcile; a Run's own Caller (Run Key) may not attest its own usage.
- **Telemetry.** The `reconciled` event exists for the exporter's `estimate_state="reconciled"` series; emitting that series is a follow-up.

## Ask First

- **O1. Totals, newest wins** (recommended) vs deltas summed across submissions. **[contract]**
- **O2. Who may submit:** Operator and Host only (recommended) vs also a Run's Caller. **[contract]**
- **O3. Budgets stay on estimated figures** (recommended) vs a reconciled figure moving a breach. 
- **O4. Conflicting replay is `409`** (recommended) vs last-write-wins under the same reference. **[contract]**

## Spec Change Log

- 2026-11-14 — spec written by the orchestrator from Story 19.5, FR-45 and AD-8 (no planning-stage spec existed; written at build time while Islam was away). O1–O4 are Islam's rulings; the build uses the recommended options and the PR stays a draft.
- 2026-11-14 — built on the recommended options:
  - **The build:** schema v26 (`run_reconciliations`), `Engine::reconcile_run` / `run_reconciliations`, `RunView.reconciled`, `hekma run reconcile`, and `POST`/`GET /v0/runs/{id}/reconciliation`.
  - **Drift:** `RunActor::Caller` is refused with `RunOutOfScope` (403).
  - **Drift:** the Control API records every submission as `operator`. The Operator and Host bearer is one credential class today, and `host` is kept for embedders that pass `RunActor::Host`.
  - **Drift:** without `--ref`, the CLI builds the reference from the figures, so a retry replays and a corrected figure is a new submission.
  - **Not built (deferred):** the OTLP exporter's `estimate_state="reconciled"` series.

