# Hardening Product Calls — draft proposals (2026-09-22)

Per AI-58 (draft-then-ratify): each item below is a COMPLETE proposal — every
option concrete, trade-offs stated, one recommended. Islam picks per item;
nothing here touches the real artifacts until ratified. These gate the five
remaining decision-shaped items on the Hardening & Housekeeping backlog
(Fibery: Hardening & Housekeeping Backlog feature).

## PC-1. Fleet detached/supervision-mode indicator

The honesty that a detached instance survives command exit lives only in the
one-time `start --detach` stderr note — `list`/`show`/`--json` carry no
detached indicator (epic-12 blind-hunter round 2).

- **(a) RECOMMENDED — persistent surface field.** Surface the spawn record's
  `detach` flag: `show --json` gains a `detached: bool` fact; the human
  `show`/Fleet lines gain an honest cell (`detached` token, `—` when
  absent-shaped). Additive `--json` keys = the announced-edit path (the 6-6
  key-set edit precedent, one announced minor). Cost: one wire field + cells;
  honors "the operator watches the surface, not the spawn note".
- (b) Re-emit the stderr note on every adoption of a detached instance.
  Cheaper, but stderr is ephemeral — the operator reading `show` still cannot
  see it.
- (c) Document-only. Rejected: the exact AI-18 shape the record criticizes.

## PC-2. pause × detach cross-lifetime semantics

A detached handle paused in-process (SIGSTOP) and then dropped strands the
child frozen and unsupervised (epic-12 edge-case-hunter): no handle exists to
resume it; the next command adopts a `paused` instance whose process is
stopped, which is at least recoverable — but until that next command, the
agent is frozen with nothing watching.

- **(a) RECOMMENDED — resume-before-release.** The disarmed `Drop` of a
  DETACHED handle whose process is SIGSTOPped resumes it (SIGCONT) before
  releasing: no command ever strands a frozen unsupervised agent. Cost: the
  backend must track paused-state per handle (it does not today — small
  addition); behavior is minimal-surprise (an agent left as we found it:
  running).
- (b) Terminate on release. A frozen detached child at drop is arguably
  unrecoverable-by-owner; killing it is honest but destructive — the operator
  asked for `--detach` survival, and the pause came from their own earlier
  command.
- (c) Document-only (today's posture). Rejected: a frozen-forever process is
  the worst silent outcome of the three.

## PC-3. The `--dump` subject-delivery proof convention (memory.dir, manifest adapters)

The subject-declared `memory.dir` delivery proof covers only the hermes
builtin; a third-party manifest adapter's own declared delivery has no proof
leg in the TCK (epic-6 retro #163/B11 residual).

- **(a) RECOMMENDED — ratify the `--dump` probe convention.** Document (in
  docs/adapter-contract.md's test-guidance + the TCK memory section) that a
  subject claiming declared memory delivery must expose the conventional
  `--dump` probe (print received environment to stdout, exit 0), and add the
  TCK memory-section subject-delivery leg that drives the subject's own dump
  seam. Contract-NEUTRAL (a test-guidance convention, not a wire change) —
  the frozen contract stays frozen; non-cooperating subjects report an honest
  `unproven` row, not a failure.
- (b) Engine-injected probe variable at start. Stronger proof, but the engine
  invents a contract token outside adapter-api — the exact Q-1 shape 6-6
  deliberately froze out.
- (c) Leave unproven. Keeps the hole the retro flagged.

## PC-4. The held engine-crates publication

Held for Islam since story 7-4 (runbook armed at docs/release-process.md,
CI pins `publish = false`). Note the new fact: this batch announced
`hekma-engine` **0.6.0** (`StoreError::MemoryBackingKindConflict`), so the
first publication would carry 0.6.0 — semver-checks-gated, banners in place.

- (a) Publish now, in runbook order (`hekma-adapter-api` →
  `hekma-adapters-hermes` → `hekma-engine`), tarball two-pass + from-crates.io
  host probe before any tag.
- **(b) RECOMMENDED — publish at the next deliberate release** (ride v0.9.1 or
  v0.10.0): the runbook is armed and the crates are consumable via the git
  pin today; bundling the publish with a tagged release gives the
  release-to-release semver loop (ci.yml's crates.io arm) a real baseline
  immediately.
- (c) Hold until the engine API stabilizes at a future major. Costs: the
  crates.io semver arm stays dormant and the deprecation shims'
  compile-against-published guarantee stays unexercised.

## PC-5. Memory attach/detach vs start TOCTOU (the AI-63(b) locking decision)

The backing row read/write and the start path's snapshot are not mutually
atomic (5-1 deferred): attach can land between a start's backing read and
spawn; consequences are bounded and self-correcting at the next stop/start.

- **(a) Route attach/detach through the supervisor mutex.** Strict
  serialization, trivially correct; cost — every attach/detach queues behind
  supervision transitions (single-operator CLI: negligible; embedding hosts
  with big fleets: contention on the coarse lock AD-18 deliberately kept
  two-mutex).
- (b) A dedicated backing lock + a cooperative re-check just before spawn.
  Finer-grained, but it is a NEW lock in the AD-18 model — the complexity the
  split study declined to add.
- **(c) RECOMMENDED — accept with a documented window.** Today's callers are
  single-operator CLI (attach and start are human-sequential); the failure
  mode self-heals at the next stop/start; the A-6 invariant (PC of record:
  kind never changes) is now store-enforced, shrinking the window's blast
  radius to a stale timestamp. Write the accepted window into
  docs/architecture.md's locking paragraph so it is a ratification, not a
  silence. Revisit only if a multi-actor host use case appears.

## How to ratify

Reply per item (e.g. "PC-1a, PC-2a, PC-3a, PC-4b, PC-5c") — ratified items
become normal backlog stories executed in the next batch; the doc is then
marked ratified and the sprint record updated in the same change.
