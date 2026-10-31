---
title: Agents in GitHub Actions
description: Run a budget-capped agent in CI — a checksum-verified release binary, a token ceiling with a breach action, a wall-clock-bounded job, usage rendered into the job summary, and exit-code honesty.
---

# Agents in GitHub Actions

A review bot is still an agent that spends money, and CI is where it runs
unattended. This page is the recipe for pointing Hekma at that agent inside a
GitHub Actions workflow: the binary is downloaded and checksum-verified, the
agent carries a token ceiling with an explicit Breach Action, the job is
bounded on the wall clock, every run lands its token and dollar usage in
`$GITHUB_STEP_SUMMARY`, and the workflow branches on stable exit codes. The
same recipe runs as a dogfood lane in this repository's own CI — the
`agents-in-ci` job in [`.github/workflows/ci.yml`](https://github.com/Ktesio/hekma/blob/main/.github/workflows/ci.yml)
exercises every mechanic below against a bundled fake agent on every pull
request, so the recipe cannot silently rot.

The contracts this page uses live in [the command reference](commands.md):
the [`agent register`](commands.md#hekma-agent-register-name---kind-kind----manifest-path)
surface, the unified [budget and rate keys](commands.md#unified-config-keys),
the [`usage`/`show` JSON documents](commands.md#hekma-agent-usage-name---json),
and the [exit codes](commands.md#exit-codes). This page points at them; it
does not replace them.

## 1. Get Hekma onto the runner

Pin a release tag and download the static musl archive with the `.sha256`
beside it, verifying before use — the same pattern the
[container recipe](hosting.md#a-worked-recipe-from-scratch) uses. Pin the tag
at copy time; the asset list on that release's page is the source of truth for
which archives exist.

```bash
HEKMA_VERSION=vX.Y.Z
archive="hekma-${HEKMA_VERSION}-x86_64-unknown-linux-musl.tar.gz"
base="https://github.com/Ktesio/hekma/releases/download/${HEKMA_VERSION}"
curl -fsSLO "${base}/${archive}"
curl -fsSLO "${base}/${archive}.sha256"
sha256sum -c "${archive}.sha256"
tar -xzf "${archive}"
```

The runner ends up with `./hekma` (and `./hkm`, the short alias). Two honesty
notes before you copy this:

- The static archive is **x86_64 Linux only** today — there is no ARM Linux
  archive yet. On a macOS or Windows runner, install through Cargo or your
  package manager instead.
- The `curl … | sh` installer always resolves the **latest** glibc release and
  has neither a version pin nor a musl target knob, so it cannot give a CI run
  a reproducible binary. Until it grows one, download the archive directly as
  above — a pinned tag plus a verified checksum is the reproducible path.

## 2. Register the agent and give it a budget

Any program a runner can start works: describe it once in an `adapter.toml`
([manifest reference](manifest.md)) and register from its directory. The
manifest below is the whole shape — how to launch the agent, that it can be
paused, and that Hekma reads its self-reported usage:

```toml
contract_version = "1.0.0"

[adapter]
kind = "review-bot"

[lifecycle.start]
exec = "/path/to/your-agent"
args = ["--ci"]

[capabilities.pause]
linux = "guaranteed"

[metering]
source = "self-reported"
```

```bash
hekma agent register review-bot --manifest ./review-bot
hekma agent config set review-bot budget.tokens.cumulative 2000000
hekma agent config set review-bot budget.breach_action pause
```

`budget.tokens.cumulative` is the spend cap for the whole job (per-run and
dollar caps exist too — see the [unified config keys](commands.md#unified-config-keys)).
`budget.breach_action` is what happens at the ceiling: `pause` (the default),
`stop`, `warn` (record only), or `block` (engine-observed instances: the
metering listener refuses further model calls; see
[the wire fence](hosting.md#the-wire-fence-block-refuses-further-model-calls)). The budget is re-read on every usage event,
so a cap set before `start` is enforced from the first event.

Agents that speak the Agent Client Protocol — most coding agents — can skip
the manifest: register with `--kind acp` and set `acp.command` (and optionally
`acp.args`) instead. Lifecycle, budgets, and usage work the same way — with
ONE CI-shaped exception: `start --detach` is refused for `acp` instances
(exit 5 — the protocol transport dies with the command that spawned it), so
an ACP agent in CI runs attached inside a step, or behind a manifest adapter
that detaches cleanly.

Optionally set prices so the ledger derives dollars:

```bash
hekma agent config set review-bot cost.rate.input 3.00
hekma agent config set review-bot cost.rate.output 15.00
```

Rates are per 1M tokens. With no rate configured, every dollar view stays
honestly inert — omitted in JSON, `—` in tables — never a fabricated `$0.00`.

## 3. Run bounded — on two clocks

Bound the job itself in the workflow:

```yaml
agents-in-ci:
  runs-on: ubuntu-latest
  timeout-minutes: 10
```

Isolate the state directory per run, or the second workflow run on a
persistent (self-hosted) runner dies at `register` with a duplicate-name
refusal:

```bash
export KTESIO_STATE_DIR="${RUNNER_TEMP}/hekma-state"
```

`RUNNER_TEMP` is fresh on every job; on ephemeral hosted runners this is
belt-and-braces, on self-hosted runners it is the difference between a
rerunnable workflow and a wedged one.

`timeout-minutes` is the user-visible wall-clock bound: a job that hits it is
cancelled by the platform, never left hanging. Inside the job, prefer Hekma's
own bounded shapes so the agent ends deterministically rather than at the
cancellation axe: start the agent detached, poll for the state you expect,
then `stop` it.

```bash
hekma agent start review-bot --detach
```

One supervision fact to design around: the CLI supervises an agent only while
some `hekma` command is running. A detached agent keeps running between
commands, but nothing meters it or enforces its budget in that window — the
next command re-adopts it and supervision resumes. Structure the job so every
check happens inside a live command session (the poll loops below), and end
the agent explicitly:

```bash
hekma agent stop review-bot --timeout 0
```

`--timeout 0` skips the graceful window and force-kills immediately — right
for CI, where a suspended (breach-paused) process cannot act on a graceful
signal anyway. A budget-breach pause is deliberately **not** lifted between
commands: a paused agent stays frozen until a later command resumes or stops
it, so a capped agent cannot run unsupervised past its ceiling.

## 4. Render the job summary from the JSON

After the run (or after the breach — see the next section), read the ledger
and write a Markdown table into `$GITHUB_STEP_SUMMARY`. The snippet is honest
by construction: dollars render only when a rate was configured, the dollar
figure carries its `estimated` label (figures are integer micro-dollars —
1,000,000 = $1.00 — so the caller formats the currency), and the cached-token
cell shows `—` unless a known non-zero cached subset exists.

```bash
hekma agent usage review-bot --json > usage.json
hekma agent show review-bot --json > show.json

in_tok=$(jq -r '.usage.cumulative_input_tokens // 0' usage.json)
out_tok=$(jq -r '.usage.cumulative_output_tokens // 0' usage.json)
cached=$(jq -r '.usage.cumulative_cached_tokens // empty' usage.json)
state=$(jq -r '.instance.state' show.json)

cost="— (no rate configured)"
if jq -e '.usage.cumulative_dollars' usage.json > /dev/null; then
  micros=$(jq -r '.usage.cumulative_dollars' usage.json)
  label=$(jq -r '.usage.estimate_label // empty' usage.json)
  cost=$(awk -v m="$micros" -v l="$label" 'BEGIN { printf "$%.6f (%s)", m / 1000000, l }')
fi
[ -n "$cached" ] && [ "$cached" -gt 0 ] || cached="—"

{
  echo "## Agent usage"
  echo ""
  echo "| agent | state | input tok | output tok | cached tok | cost |"
  echo "|-------|-------|-----------|------------|------------|------|"
  printf '| %s | %s | %s | %s | %s | %s |\n' review-bot "$state" "$in_tok" "$out_tok" "$cached" "$cost"
} >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
```

`cumulative_dollars` and `estimate_label` are optional fields: a parser must
treat their absence as "no rate", never as zero. When dollars are present the
frozen surface always carries the label beside them (the pair is
test-pinned), so the snippet reads the label without a fallback — an
unlabeled dollar figure is a bug worth failing on, not a value to invent. The
same discipline applies
to the cached subset — absent means unknown, `0` means known-zero, and the
summary shows the number only when there is one worth showing.

## 5. Branch on exit codes — and read a breach from the row, not a code

Every `hekma` command returns one of the [documented exit codes](commands.md#exit-codes),
so the workflow branches without parsing stderr:

```bash
set +e
hekma agent start review-bot --detach
code=$?
set -e
case $code in
  0) echo "agent started (detached)" ;;
  2|3) echo "invocation or name error (exit $code) — fix the workflow" >&2; exit 1 ;;
  4|5) echo "state or capability refusal (exit $code) — see stderr (note: --detach is refused for --kind acp)" >&2; exit 1 ;;
  6) echo "a bounded operation timed out (exit 6)" >&2; exit 1 ;;
  *) echo "unexpected failure (exit $code)" >&2; exit 1 ;;
esac
```

**There is no dedicated breach exit code.** The stable surface is `0`–`6`, and
none of them means "the budget was crossed" — a breach is not a command
failure, it is a state the agent lands in. Prove a breach by reading the
Fleet row: the state reads `paused` (or `stopped`, under `breach_action
stop`) and the budget block carries the ceiling with `cumulative_remaining`
clamped at `0`. Spend checks — "did the bot stay under budget?" — read
`usage --json` totals against your own ceiling instead.

The poll loop that proves the breach is bounded and fails loudly, naming the
last read instead of just timing out:

```bash
paused=""
for i in $(seq 1 60); do
  hekma agent show review-bot --json > show.json || { sleep 1; continue; }
  state=$(jq -r '.instance.state // empty' show.json) || { sleep 1; continue; }
  limit=$(jq -r '.instance.budget.cumulative_limit // empty' show.json)
  remaining=$(jq -r '.instance.budget.cumulative_remaining // empty' show.json)
  if [ "$state" = "paused" ] && [ -n "$limit" ] && [ "$remaining" = "0" ]; then paused=1; break; fi
  sleep 1
done
if [ -z "$paused" ]; then
  echo "::error::the agent never reached a budget-paused row within the bound; last read:" >&2
  cat show.json >&2
  exit 1
fi
```

The `remaining = 0` conjunction matters: an agent paused for any OTHER reason
(an operator pause, a crash-window state) still reads `paused`, but only a
budget breach clamps the remaining allowance to zero — the row proves the
breach, not just the pause.

When the row proves the pause, name the breach in the summary — the same
table as the success case, with the state column carrying the story:

```bash
{
  echo "## Agent usage — BUDGET BREACH"
  echo ""
  echo "review-bot crossed its cumulative ceiling of ${limit} tokens and was"
  echo "paused by the configured Breach Action. See the row below."
} >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
```

Keep the breach leg's `continue-on-error: false` (the GitHub default): a leg
that proves a breach succeeds — the agent did exactly what the budget said —
while a leg that *fails to prove* its expected outcome fails the job.

## 6. Wire secrets as references, never literals

Auth-gated agents get their credentials through Hekma's secret-reference
form: a config value of the form `secret:NAME` is stored as the *reference*
and resolved from the environment (then the state root's secrets file) at
`start`, masked everywhere Hekma displays it. The workflow carries the
material only as a GitHub secret mapped into the step's environment:

```yaml
steps:
  - name: Run the governed agent
    env:
      AGENT_API_KEY: ${{ secrets.AGENT_API_KEY }}
    run: |
      hekma agent config set review-bot agent.api_key secret:AGENT_API_KEY
      hekma agent start review-bot --detach
```

Never inline a key in the YAML, a manifest, or a `config set` value. Prefer
env or file targets in the adapter's config mapping: a key mapped onto a
command-line **flag** rides the agent's argv, where other local users can
read it — Hekma warns when a `secret:` value would land there.

## The dogfood lane in this repository

The `agents-in-ci` job in this repo's CI runs this exact recipe on every pull
request, with two deliberate differences from the copy-paste path above:

- **It builds the branch's own binaries** (`hekma` + the bundled `fake_agent`
  helper) instead of downloading a release, so the lane tests the commit under
  review with no network egress. Your workflow wants the release download from
  step 1; the lane's build step is the only part you should not copy.
- **The agent is the bundled `fake_agent`** emitting a fixed, known batch —
  5 usage events of 10 input + 20 output tokens — so both legs are
  deterministic: a **success leg** (ceiling far above the batch, exit 0,
  tokens and labeled dollars rendered) and a **breach leg** (ceiling 90, the
  batch crosses it at event 3, the agent pauses, the poll proves the paused
  row plus the budget fields, and the summary names the breach). Every wait
  polls for a known committed state — never a sleep.

The job is capped at `timeout-minutes: 15`, runs on `ubuntu-latest` with only
`contents: read`, needs no secrets (the fake agent calls nothing), and writes
both legs' tables into the job summary — the lane is the recipe's own test.
