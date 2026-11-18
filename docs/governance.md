---
title: Governance Conformance Vectors
description: The versioned, checksummed decision artifact a Host pins to prove its external budget/cost guard matches the engine's governance rules.
---

# Governance Conformance Vectors

Hekma's budget and cost guardrails follow fixed, public rules: a ceiling is
breached **at or above** the limit, a `(dimension, scope)` pair fires **once
per Run**, token ceilings are evaluated **before** dollar caps, money is
derived in **integer micro-dollars** with **round-half-up** division,
arithmetic **saturates** instead of wrapping, and each ledger row is priced at
the rate persisted **with** it (no retroactive repricing). A Host that
enforces money at its own choke point — outside Hekma — needs proof that its
guard implements exactly these rules, not an approximation of them.

The **governance conformance vectors** are that proof, as data: a versioned,
SHA-256-checksummed JSON document of worked cases generated from the engine's
own decision code. Every case is a concrete input (ledger state, ceilings,
latch snapshot) with the exact expected output, so a Host can re-run the cases
against its implementation and assert byte-exact agreement.

- Artifact id: `hekma-governance-vectors`
- Canonical location: `crates/hekma-engine/governance/vectors-v1.json`
- Checksum sidecar: `crates/hekma-engine/governance/vectors-v1.json.sha256`
- Current version: **1.4.0** (schema version 1). 1.1.0 appended the `block`
  cases and the `fence` kind after the unchanged 1.0.0 set; 1.2.0 appended the
  `chain` kind for ancestor ceilings after the unchanged 1.1.0 set; 1.3.0
  appended the `policy` kind for Policy resolution after the unchanged 1.2.0
  set; 1.4.0 appended the `rearm` kind for the within-Run re-arm after the
  unchanged 1.3.0 set, then six `policy-caller-*` cases for the resolver's
  Caller layer (a Run's own limits, applied only where tighter), then four
  `policy-wire-*` cases: `block` executes at every depth with a wire path,
  and so do `pause` (holding the Run's calls) and `stop` (refusing them),
  then three Scent-Check Mode cases, then three `policy-observed-*`
  cases: at `observed` depth (no wire path, no process) a Policy's `warn`
  executes, while `block` and `pause` act as `warn` with the
  `depth-fallback` Anomaly.

## Getting the artifact

Print the human summary (version, checksum, case mix):

```bash
hekma agent governance-vectors
```

Print the raw canonical bytes — byte-for-byte the released artifact, suitable
for piping into a checksum:

```bash
hekma agent governance-vectors --json | sha256sum
```

The same two files also ship as release assets (`vectors-v1.json` and
`vectors-v1.json.sha256` in every release's attachments, and inside the
`hekma-<version>-checksums.txt` manifest), so a Host can pin a release
without running the CLI.

## The document, schema version 1

An excerpt (the full artifact carries 79 cases; the shape is identical
throughout):

```json
{
  "artifact": "hekma-governance-vectors",
  "schema_version": 1,
  "vectors_version": "1.4.0",
  "provenance": {
    "generated_from": [
      "BudgetEvaluator",
      "CostEvaluator",
      "cost_micros",
      "cost_micros_with_cached",
      "governance::decide",
      "governance::fence_refusal",
      "governance::decide_chain",
      "runs::policy::resolve",
      "governance::rearm"
    ]
  },
  "cases": [
    {
      "kind": "decision",
      "id": "decision-threshold-ge-exact-at-ceiling",
      "covers": ["threshold-ge"],
      "input": {
        "token": {
          "run_total": 0,
          "cumulative_total": 100,
          "budget": { "per_run": null, "cumulative": 100 },
          "action": "pause"
        },
        "latched": []
      },
      "expected": {
        "decisions": [
          {
            "dimension": "tokens",
            "scope": "cumulative",
            "action": "pause",
            "limit": 100,
            "observed": 100
          }
        ],
        "latched_after": [{ "dimension": "tokens", "scope": "cumulative" }]
      }
    }
  ]
}
```

Six case kinds exist:

- **`decision`** — a full governance evaluation: arms (the armed dimensions,
  with committed totals, ceilings, and the resolved breach action), the
  per-Run latch snapshot in, and the fired decisions out in fire order (token
  before dollar) plus the latch state after.
- **`derivation`** — a pure cost derivation: token counts and rates in, the
  expected cost in integer micro-dollars out. Three derivations are covered:
  `cost-micros` (plain per-direction pricing), `cost-micros-with-cached` (the
  cached re-split), and `two-row-own-rate-sum` (two ledger rows priced each at
  its own persisted rate).
- **`fence`** (from 1.1.0) — the `block` wire fence: the raised fence reasons
  (each `(dimension, scope)` whose breach fired `block`) in, and the refusal
  out: `status` 402, `error_type` `hekma_budget_block`, the `code`, and the
  reported reason, or `null` when no reason is raised. The response these
  fields come from is described in
  [Hosting: the wire fence](hosting.md#the-refusal-proxy-wire-v1).

- **`chain`** (from 1.2.0) — ancestor ceilings: one `decision`-shaped input
  per level of a Run's ancestor chain (the Run a usage event is attributed to
  first, the Run that owns the process last), each with that level's arms over
  its own subtree totals and its own latches, and one outcome per level in the
  same order. Every ancestor's ceilings apply: each level decides on its own,
  so a child's breach never suppresses its parent's, and a parent's ceiling
  binds its children's usage. A one-level chain is exactly a `decision`.

- **`policy`** (from 1.3.0) — Policy resolution: the Run's depth, wire path
  and bound labels, its explicit limits, the policy set and the model in, and
  the exact resolution out — the effective ceilings and action with each
  limit's source, the depth fallback, and the unpriced-cap signal.

- **`rearm`** (from 1.4.0) — the within-Run re-arm after a limit change: the
  level's ceilings after the change (`arms`, with `arms_before` as context
  only), its effective action, whether a Rate resolves, its committed totals
  (the Run's own for the per-run scopes, the agent's lifetime totals for the
  cumulative ones) and its latches before the change, in; out, the latches
  released (`unlatched`), the `block` fence reasons lifted
  (`fence_cleared`), the breaches the change fires at once (`fired`), the
  latches and fence reasons after, and whether the Run's `breach` Anomaly
  holds. `trigger` names what moved the ceiling — a Policy change
  (`policy-change`), a limit set on the Run (`run-limits`) or an agent
  configuration write (`config-write`). It is context, not an input: the rule
  never reads it, so the expected outcome never depends on it, and three
  cases carry the same input under all three so a consumer that branched on
  it would fail. These cases pin the rule, not the engine's three paths to
  it; that each path hands the rule the same input is the engine's own
  tested contract.

A later minor version may add a case kind, as 1.1.0 added `fence`, 1.2.0
added `chain`, 1.3.0 added `policy` and 1.4.0 added `rearm`. See the
versioning policy for how a consumer pinned to an older version handles it.

Every case carries `covers` — the rule set it pins:

| Tag | Rule |
| --- | --- |
| `threshold-ge` | A ceiling breaches at or above the limit — exactly-at breaches, one-below does not, a zero ceiling breaches on zero. |
| `latch-per-scope` | An already-fired `(dimension, scope)` pair never re-fires; the token and dollar dimensions of the same scope are independent. |
| `latch-re-arm` | A fresh latch at a Run boundary plus the same totals fires again. |
| `token-before-dollar` | With both dimensions armed and both tripping, the token decision is reported first. |
| `integer-micros` | Money is integer micro-dollars throughout: per-direction rates, the cached re-split, no floating point, no wrap. |
| `round-half-up` | A token×rate divide rounds half-up; each direction rounds independently — pinned by a both-directions-at-the-tie case that an aggregate-then-round implementation fails (4 vs 3). |
| `saturation` | A runaway product pins at the integer maximum — never wraps, so a cost can never silently un-breach. |
| `no-retro-repricing` | Each ledger row prices at the rate persisted with it; repricing history at the latest rate produces a different (wrong) value. |
| `block-fence` | A `block` decision fires under the same threshold, latch and order rules as any action. A fenced Run refuses with status 402 and the `hekma_budget_block` class, reporting the first raised reason: tokens before dollars, per-run before cumulative. |
| `ancestor-ceilings` | Over a Run's ancestor chain every level decides on its own subtree totals and its own latches; outcomes come deepest level first, token before dollar within a level. |
| `re-arm` | After a limit change, a latch whose ceiling is now above its total, or no longer set, re-arms, and its `block` fence reason lifts per reason — a fence holding another latched `block` reason stays up. A ceiling at or below its total keeps its latch, and one lowered to at or below the total fires at once. A latch is keyed by the action that recorded it, so a kept pair fires once more under a new action. The outcome is the same whatever moved the ceiling. |
| `policy-resolution` | Each limit resolves on its own: explicit limits on the Run, then on the agent, outrank every Policy; per limit the most specific Selector (a strict term-set superset) wins, incomparable Selectors tie to the tightest value, and the default Policy fills what is unset. A malformed action fails closed to `pause`; an action the depth cannot execute acts as `warn`; a Cost Cap with no Rate is loud, never silently inert. |

## How a Host pins it

1. **Pin the version + checksum.** Record the `vectors_version` you verified
   against and the SHA-256 of the canonical bytes (the published `.sha256`
   sidecar, or `hekma agent governance-vectors --json | sha256sum`).
2. **Re-run every case.** For each `decision` case, feed `input` through your
   guard and compare the fired decisions (order included) and the resulting
   latch state to `expected`. For each `derivation` case, run the named
   derivation on the inputs and compare the micro-dollar result to
   `expected_micros`. For each `chain` case, run every level through your
   guard and compare the outcomes level by level. For each `policy` case,
   resolve the input with your own resolution and compare every field of the
   expected resolution. For each `rearm` case, release the latches whose
   ceiling is now above the total (or unset), lift their `block` reasons,
   then run your guard over the new arms with the kept latches under the
   current action, and compare every field of `expected`.
3. **Fail on any difference.** A mismatch means your guard disagrees with the
   engine on a pinned rule — fix the guard (or file an issue if you believe
   the vector is wrong), never silently skip the case.
4. **On upgrade, re-run.** A new minor version appends cases after the ones
   you already pass; your existing verdicts stay valid and the new cases
   extend the proof. See the versioning policy below.

A Rust Host can skip the re-implementation entirely: the engine exposes the
same decision core (`governance::decide`, `decide_chain`, `rearm`, the derivations) through its public
API — see [Embedding the engine](embedding.md).

## Versioning policy

`vectors_version` is semver over the **case set**:

- **Additive cases** are a **minor** bump: new scenarios pinning the same
  rules, or cases for new rules. A new case may use a new case kind or a new
  value of an existing field (1.1.0 added the `fence` kind, the `block` action
  and the `block-fence` tag; 1.2.0 the `chain` kind and the
  `ancestor-ceilings` tag). New cases are **appended**: every case an
  earlier version shipped keeps its bytes and its position. A consumer pinned
  to an earlier version re-runs that version's cases, the leading prefix of
  the list, and parses the rest only when it adopts the new version. Its
  existing re-run stays green; adopting the new cases is recommended but not
  required.

  | Version | Cases | Added |
  | --- | --- | --- |
  | 1.0.0 | 1-26 | the first 26 `decision` and `derivation` cases |
  | 1.1.0 | 27-34 | the `decision-block-*` cases and the `fence-*` cases |
  | 1.2.0 | 35-39 | the `chain-*` cases |
  | 1.3.0 | 40-53 | the `policy-*` cases and two `decision-policy-*` cases |
  | 1.4.0 | 54-76 | the `rearm-*` cases, then the `policy-caller-*` cases, then the `policy-wire-*` cases, then the Scent-Check `policy-*` cases (a mode tie, a Caller tighten, a looser Caller mode ignored) |

  The provenance list grows with the generator functions the new cases come
  from (1.1.0 added `governance::fence_refusal`, 1.2.0
  `governance::decide_chain`, 1.3.0 `runs::policy::resolve`, 1.4.0
  `governance::rearm`).
- **Any change to a shipped case** (its expected output or its input), or to
  the shape of the header or of an existing kind, is a **major** bump and is announced ahead of the release in the release
  notes. A major bump means a pinned verdict must be re-established — it does
  not by itself mean the enforcement behavior changed; it means the *proof*
  changed.

The checksum protects transport, not semantics: matching bytes mean you have
the exact artifact a release carried; the version tells you what its cases
promise. The provenance block records the evaluated symbols — the engine
functions each artifact was generated from and verified against. It names
GENERATOR FUNCTIONS, not a crate version: the crate version lives in the
changelog and is not a pin key (a Host pins `vectors_version` + the SHA-256,
which already cover the exact bytes).

## Parsing the integers (a note for dynamically-typed hosts)

Every numeric field in the artifact is a JSON integer, and two of the pinned
rules are only visible at the EXTREMES: the saturation cases deliberately
carry values beyond 2^53 (beyond what a JavaScript `Number` can represent
losslessly). A host that parses them through a float type will silently
corrupt the case and can pass or fail for the wrong reason. Parse every
integer losslessly — in JavaScript, use a BigInt-aware JSON parser (or
`JSON.parse` with a reviver over the numeric fields). The fields that carry
extremes:

- `input_tokens` / `output_tokens` — `u64::MAX` in the saturation cases
  (`derivation-saturation-max-tokens-max-rate`,
  `derivation-saturation-cached-derivation-saturates`).
- `rate.input_micros_per_1m` / `rate.output_micros_per_1m` /
  `rate.cached_micros_per_1m` — `u64::MAX` in the same cases.
- `expected_micros` — `i64::MAX` (the saturation pin itself).

## What the vectors are not

They describe **decisions**, never enforcement power. Hekma is a cooperative
meter: its supervision pauses an agent through lifecycle actions, and a Host
consuming the vectors re-implements the decision rules at its own choke
point. The artifact carries no sandbox, no process control, and no
cryptographic attestation — it is a public, deterministic transcript of the
rules, and the checksum's role is tamper-evidence in transit, not secrecy.

The budget/cost behavior these cases pin is the one documented in the
[command reference](commands.md#budget-breaches-and-the-pause-action): the
same thresholds, the same once-per-Run-per-scope latch, the same
token-before-dollar precedence. The vectors add a portable, checkable form —
they do not define a second behavior.

## Regenerating (maintainers)

The artifact is generated from the engine's own evaluators and verified on
every test run: a semantic change to any decision or derivation makes the
committed file stale and the verifier red. After a **ratified** semantic
change, regenerate both files and review the diff case by case:

```bash
HEKMA_UPDATE_GOVERNANCE_VECTORS=1 cargo test -p hekma-engine governance
sha256sum -c crates/hekma-engine/governance/vectors-v1.json.sha256
```

On macOS, `sha256sum` may be absent — `shasum -a 256` is the same digest:

```bash
shasum -a 256 -c crates/hekma-engine/governance/vectors-v1.json.sha256
```

The bless run exits RED on the sibling verifiers in the same run: the
compile-time EMBEDDED copy (`include_str!`) is the pre-bless bytes until the
crate is rebuilt, so the byte-compare and checksum tests that execute after
the write still see stale input. That red is expected — the honest two-step:

1. bless (the command above; it rewrites the artifact + sidecar, then the
   remaining tests in that run may fail against the stale embed), and
2. re-run the same suite on a fresh build — it must be GREEN before the
   change lands.

The regenerated artifact must land in the same change as the semantic change
that produced it, with the version bump the policy above calls for.
