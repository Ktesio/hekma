---
title: Metering agents you don't control
description: Why Ktesio meters an agent's model traffic at a boundary the agent cannot bypass, how the loopback proxy, the Usage Ledger, and budget enforcement fit together, and what the meter still cannot see.
---

# Metering agents you don't control

*Islam Magdy — creator of Ktesio · 7 September 2026*

A long-running AI agent is a process that spends money every time it wakes up. By agent I mean a program you run for yourself that calls a model on your behalf: a personal agent such as Hermes Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI. One that loops, retries, or gets stuck in a tool cycle can write thousands of lines on next month's invoice before anyone looks.

What is new is how casually we run them. A service gets a supervisor, a restart policy, logs you can read after it dies, and a metric somebody is paged on. An agent gets a terminal window and a shell script. Nobody can stop it cleanly, nobody knows what it consumed in the last hour, and the first honest accounting arrives weeks later on a bill. I built Ktesio to run agents with the discipline I expect from services: start them, stop them, pause them, and know what they cost while they run, not after.

The lifecycle half is unglamorous supervision work. This essay is about the metering half.

## The honor system

The first design everyone reaches for, me included, is to let the agent tell you what it spent. The SDK exposes a usage callback, the framework logs a usage line, the agent prints a summary at the end of a run, and you call the sum a ledger.

An adapter can declare `self-reported`, and the agent emits a `KTESIO_USAGE` line on its stdout carrying a sequence number and the input and output token counts. The sequence number lets me recognize a replayed batch and refuse to count it twice.

I would not build a cost policy on it, and here is why. Agents crash mid-run, and the summary that would have carried the count is exactly the line that never gets written. Agents retry silently: the provider returns an error, the client library retries three times, and the usage callback fires once, for the response that finally came back. Agents call tools outside the reporting loop, and a tool that summarizes a document through its own model client is invisible to the outer callback. And third-party adapters report whatever they choose. One agent I evaluated while freezing the [adapter contract](../adapter-contract.md) coerces missing usage to zero instead of saying it does not know. Zero looks like a number. It is not one.

Add these up and self-reported metering is an honor system. That is fine for an agent you wrote, running code you can read. It is a strange foundation for a budget whose job is to stop a process you do not control from spending money you have not approved.

## Put the meter on the wire

Here is the claim the rest of Ktesio's cost governance rests on: cost governance cannot depend on the agent's cooperation. The number a budget acts on has to come from a boundary the agent cannot bypass, cannot forget to update, and cannot round down to zero.

The electricity meter is the right picture. The utility does not ask the appliance how much it drew. The meter sits on the wire, between the appliance and the supply, and counts what actually passed. A broken appliance, a lying appliance, and a well-behaved appliance are metered the same way, because the meter never asked their opinion. And if you want to stop the appliance, you cut the wire, from the meter's side.

For an agent, the wire is its model traffic. Every token a provider bills for crosses an HTTP connection to that provider. If the engine sits on that connection, it can count what actually crossed without a single line of cooperation from the agent.

## How Ktesio does it

An agent is registered through an [adapter manifest](../manifest.md), and the manifest must declare a metering source. There is no `none`: an adapter without a viable metering source is rejected at registration, before any state is written.

For `engine-observed`, the engine runs the meter itself. At the `starting` transition it binds a loopback listener on `127.0.0.1:0`, an ephemeral loopback port. It refuses any non-loopback address, and nobody but the engine chooses it. The engine injects the resulting `http://127.0.0.1:<port>` through a reserved [config key](../commands.md#unified-config-keys), `metering.base_url`, which the adapter maps into whatever the agent reads for its OpenAI-compatible endpoint, typically an environment variable such as `OPENAI_BASE_URL`. The operator points `metering.upstream_base_url` at the real provider. The agent believes it is talking to its provider. It is talking to the meter.

The listener is a transparent forward proxy. It forwards the method, path, query, and headers verbatim, including the agent's own `Authorization` header, so the agent's key flows upstream untouched. It relays the status, headers, and body back unchanged; an unreachable upstream gets an honest `502`. On the way back it reads the body once, skims `usage.prompt_tokens` and `usage.completion_tokens`, and pushes those two integers onto a queue. Nothing else leaves the proxy: no body, header, URL, or key reaches a log, an error, or a ledger row, and a sentinel-key test proves it.

Both sources end in the same place. The supervisor drains the queue on its reaper tick and hands each count to `ingest_usage`, the one function allowed to write the Usage Ledger: an append-only `usage_events` table in the engine's SQLite database, one committed transaction per event. Each row carries the instance, the Run id, both token counts, the metering source, a timestamp, and a sequence number, under a unique index so a replayed event is a no-op. A Run spans one `starting` transition to the next terminal state; per-run totals cover that span, and cumulative totals sum every row the instance ever wrote. An observed agent supplies no sequence, so the engine mints one per completion.

Budgets are ordinary config keys, changeable while the agent runs:

```bash
kt agent config set my-agent budget.tokens.cumulative 500000
kt agent config set my-agent budget.breach_action pause
kt agent config set my-agent cost.rate.input 3.00
kt agent config set my-agent cost.rate.output 15.00
kt agent config set my-agent budget.dollars.cumulative 10.00
```

Where enforcement runs matters most to me. Immediately after a fresh row commits, in the same synchronous call, `ingest_usage` re-reads the current budget from config, reads the just-committed per-run and cumulative totals, and runs a pure evaluator over them. Tokens are checked first, per-run before cumulative, at a `>=` threshold: reaching the ceiling is the breach. Then, if a rate exists, the dollar cap is checked the same way. On a breach the supervisor writes a breach event to a durable per-instance log before anything else, then executes the action: `pause`, `stop`, or `warn`. Pause is the default, and a breach fires once per dimension and scope per Run.

The obvious alternative deserves a fair hearing. A separate watcher that reads the ledger every second and pauses anything over its ceiling is simpler, and it keeps ingestion fast and dumb. I rejected it because it opens a window between "usage recorded" and "budget checked" in which the total is over the line and nobody has acted. Putting the evaluator inside the commit path closes the window by construction. The price is a config read and two comparisons on every ingested event. I will pay that.

Money gets the same suspicion. Rates are dollars per million tokens, stored as integer micro-dollars, never floats. Each row is priced at the rate in force when it committed, so changing the rate never rewrites history. Every dollar figure Ktesio renders comes out of one module and carries the label `estimated`, and a CI lint fails the build if any other module formats a dollar. Running `kt agent usage my-agent --json` prints the ledger's view of that instance:

```json
{
  "schema_version": 2,
  "instance": "my-agent",
  "usage": {
    "cumulative_input_tokens": 1200,
    "cumulative_output_tokens": 3400,
    "current_run_input_tokens": 120,
    "current_run_output_tokens": 340,
    "cumulative_dollars": 54600,
    "current_run_dollars": 5460,
    "estimate_label": "estimated"
  }
}
```

Those token totals are the ledger sums exactly, the same numbers `kt agent list` and `kt agent show` print. With no rate configured the dollar fields are absent rather than `0`. The code is [source-available](https://github.com/iMagdy/ktesio), so every sentence in this section can be checked against it.

## What the meter can't see

A meter is only as good as the wire it sits on, and today the wire has gaps. I would rather list them here than have you find them on an invoice.

The proxy sees only traffic the agent sends to the injected address. If the adapter maps `metering.base_url` nowhere, if the agent ignores the environment variable, or if it calls a second endpoint, an embeddings API, or a tool with its own model client, none of that is metered and nothing warns you. The totals simply stay low, and a low total looks like good behavior.

The upstream must be plain `http://` today; an `https://` upstream is refused at start with a message that says so. I shipped without a TLS stack to keep the dependency tree small and free of a C build, and I still think that was the right first cut, but the consequence is real: to meter an agent that talks to a hosted provider you need a local hop that speaks plain HTTP to Ktesio and TLS to the provider.

Only non-streaming JSON responses are parsed. A streamed completion is relayed faithfully and not metered at all: its usage arrives in a final server-sent event chunk, only when the client asked for it, and I have not written that parser yet. The same goes for a response with no `usage` object, a malformed one, or a provider whose usage schema is not OpenAI-shaped: the call succeeds and the ledger misses it. I buffer whole bodies, capped at 64 MiB, because a faithful relay plus one JSON parse is a small amount of code I can test exhaustively, and a streaming state machine is not. So read an observed instance's totals as a lower bound.

*Update 2026-09-15 (AI-47 ratified, epic-12): the streaming usage parse lands first (story 12-2 — the terminal `include_usage` SSE frame), then an HTTPS upstream via vendored rustls+ring (story 12-3). Until those ship, the paragraphs above describe shipped behavior exactly.*

The parser reads two fields and ignores the rest. Cached input tokens, reasoning tokens, and provider pricing tiers are not modeled, so the dollar figure is an estimate from a flat per-direction rate and will not match the provider's invoice. A `reconciled` label exists in the type, but no code produces it today.

Enforcement acts on committed rows, and rows commit on a 250 millisecond reaper tick. Between the response that crossed the ceiling and the pause, the agent may have more requests in flight, and those reach the provider. A pause is only as strong as the adapter's declaration: `guaranteed` on Unix is a real signal stop of the whole process group, while `best-effort` records the state change with a visible qualifier and sends nothing to the process, which may keep running. The native Hermes adapter declares best-effort everywhere, because that gateway has no freeze mechanism. `warn` records the breach and does nothing else. A budget is a ceiling on what the ledger has seen, not a fence around what the agent can spend.

The loopback bind guards against accidental network exposure of the agent's traffic and key. It is not a sandbox, and an Agent Home is process and filesystem isolation, nothing stronger.

Finally, the listener lives with the engine. A standalone `kt agent start` supervises the process only for that command's lifetime, and the listener dies with it. If the engine crashes and a later open re-adopts a surviving observed agent, that agent's `base_url` still points at a dead port, and its model calls fail with a connection error until you stop and start it. That fails loud, never wrong, but it is a gap, and the supervising daemon that would close it is still on the roadmap. Self-reported instances have no listener and are unaffected, which is an irony I have not fixed yet.

An observed agent's numbers are numbers it could not have faked, and they are a lower bound on what it spent.

Run agents like services. Give them a supervisor, a ledger you did not ask them to fill in, and a ceiling that pauses them before the invoice does. That is all Ktesio is trying to be, and where it falls short today, it says so.
