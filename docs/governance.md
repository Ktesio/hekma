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
- Current version: **1.0.0** (schema version 1)

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

An excerpt (the full artifact carries 20 cases; the shape is identical
throughout):

```json
{
  "artifact": "hekma-governance-vectors",
  "schema_version": 1,
  "vectors_version": "1.0.0",
  "provenance": {
    "engine_version": "0.9.0",
    "generated_from": [
      "BudgetEvaluator",
      "CostEvaluator",
      "cost_micros",
      "cost_micros_with_cached",
      "governance::decide"
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

Two case kinds exist:

- **`decision`** — a full governance evaluation: arms (the armed dimensions,
  with committed totals, ceilings, and the resolved breach action), the
  per-Run latch snapshot in, and the fired decisions out in fire order (token
  before dollar) plus the latch state after.
- **`derivation`** — a pure cost derivation: token counts and rates in, the
  expected cost in integer micro-dollars out. Three derivations are covered:
  `cost-micros` (plain per-direction pricing), `cost-micros-with-cached` (the
  cached re-split), and `two-row-own-rate-sum` (two ledger rows priced each at
  its own persisted rate).

Every case carries `covers` — the rule set it pins:

| Tag | Rule |
| --- | --- |
| `threshold-ge` | A ceiling breaches at or above the limit — exactly-at breaches, one-below does not, a zero ceiling breaches on zero. |
| `latch-per-scope` | An already-fired `(dimension, scope)` pair never re-fires; the token and dollar dimensions of the same scope are independent. |
| `latch-re-arm` | A fresh latch at a Run boundary plus the same totals fires again. |
| `token-before-dollar` | With both dimensions armed and both tripping, the token decision is reported first. |
| `integer-micros` | Money is integer micro-dollars throughout: per-direction rates, the cached re-split, no floating point, no wrap. |
| `round-half-up` | A token×rate divide rounds half-up; each direction rounds independently. |
| `saturation` | A runaway product pins at the integer maximum — never wraps, so a cost can never silently un-breach. |
| `no-retro-repricing` | Each ledger row prices at the rate persisted with it; repricing history at the latest rate produces a different (wrong) value. |

## How a Host pins it

1. **Pin the version + checksum.** Record the `vectors_version` you verified
   against and the SHA-256 of the canonical bytes (the published `.sha256`
   sidecar, or `hekma agent governance-vectors --json | sha256sum`).
2. **Re-run every case.** For each `decision` case, feed `input` through your
   guard and compare the fired decisions (order included) and the resulting
   latch state to `expected`. For each `derivation` case, run the named
   derivation on the inputs and compare the micro-dollar result to
   `expected_micros`.
3. **Fail on any difference.** A mismatch means your guard disagrees with the
   engine on a pinned rule — fix the guard (or file an issue if you believe
   the vector is wrong), never silently skip the case.
4. **On upgrade, re-run.** A new minor version adds cases; your existing
   verdicts stay valid and the new cases extend the proof. See the
   versioning policy below.

A Rust Host can skip the re-implementation entirely: the engine exposes the
same decision core (`governance::decide`, the derivations) through its public
API — see [Embedding the engine](embedding.md).

## Versioning policy

`vectors_version` is semver over the **case set**:

- **Additive cases** (new scenarios pinning the same rules, or cases for new
  rules) are a **minor** bump. A consumer's existing re-run stays green;
  adopting the new cases is recommended but not required.
- **Any change to an expected output, a case input, or the document schema**
  is a **major** bump and is announced ahead of the release in the release
  notes. A major bump means a pinned verdict must be re-established — it does
  not by itself mean the enforcement behavior changed; it means the *proof*
  changed.

The checksum protects transport, not semantics: matching bytes mean you have
the exact artifact a release carried; the version tells you what its cases
promise. The provenance block records the engine version and the evaluated
symbols each artifact was generated from.

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

The regenerated artifact must land in the same change as the semantic change
that produced it, with the version bump the policy above calls for.
