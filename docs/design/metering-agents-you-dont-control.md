---
title: Metering agents you don't control
description: Why Hemaka meters an agent's model traffic at a boundary the agent cannot bypass, how the loopback proxy, the Usage Ledger, and budget enforcement fit together, and what the meter still cannot see.
---

# Metering agents you don't control

*Islam Magdy — creator of Hemaka · 7 September 2026*

A long-running AI agent is a process that spends money every time it wakes up. By agent I mean a program you run for yourself that calls a model on your behalf: a personal agent such as Hermes Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI. One that loops, retries, or gets stuck in a tool cycle can write thousands of lines on next month's invoice before anyone looks.

What is new is how casually we run them. A service gets a supervisor, a restart policy, logs you can read after it dies, and a metric somebody is paged on. An agent gets a terminal window and a shell script. Nobody can stop it cleanly, nobody knows what it consumed in the last hour, and the first honest accounting arrives weeks later on a bill. I built Hemaka to run agents with the discipline I expect from services: start them, stop them, pause them, and know what they cost while they run, not after.

The lifecycle half is unglamorous supervision work. This essay is about the metering half.

## The honor system

The first design everyone reaches for, me included, is to let the agent tell you what it spent. The SDK exposes a usage callback, the framework logs a usage line, the agent prints a summary at the end of a run, and you call the sum a ledger.

An adapter can declare `self-reported`, and the agent emits a `KTESIO_USAGE` line on its stdout carrying a sequence number and the input and output token counts. The sequence number lets me recognize a replayed batch and refuse to count it twice.

I would not build a cost policy on it, and here is why. Agents crash mid-run, and the summary that would have carried the count is exactly the line that never gets written. Agents retry silently: the provider returns an error, the client library retries three times, and the usage callback fires once, for the response that finally came back. Agents call tools outside the reporting loop, and a tool that summarizes a document through its own model client is invisible to the outer callback. And third-party adapters report whatever they choose. One agent I evaluated while freezing the [adapter contract](../adapter-contract.md) coerces missing usage to zero instead of saying it does not know. Zero looks like a number. It is not one.

Add these up and self-reported metering is an honor system. That is fine for an agent you wrote, running code you can read. It is a strange foundation for a budget whose job is to stop a process you do not control from spending money you have not approved.

## Put the meter on the wire

Here is the claim the rest of Hemaka's cost governance rests on: cost governance cannot depend on the agent's cooperation. The number a budget acts on has to come from a boundary the agent cannot bypass, cannot forget to update, and cannot round down to zero.

The electricity meter is the right picture. The utility does not ask the appliance how much it drew. The meter sits on the wire, between the appliance and the supply, and counts what actually passed. A broken appliance, a lying appliance, and a well-behaved appliance are metered the same way, because the meter never asked their opinion. And if you want to stop the appliance, you cut the wire, from the meter's side.

For an agent, the wire is its model traffic. Every token a provider bills for crosses an HTTP connection to that provider. If the engine sits on that connection, it can count what actually crossed without a single line of cooperation from the agent.

## How Hemaka does it

An agent is registered through an [adapter manifest](../manifest.md), and the manifest must declare a metering source. There is no `none`: an adapter without a viable metering source is rejected at registration, before any state is written.

For `engine-observed`, the engine runs the meter itself. At the `starting` transition it binds a loopback listener on `127.0.0.1:0`, an ephemeral loopback port. It refuses any non-loopback address, and nobody but the engine chooses it. The engine injects the resulting `http://127.0.0.1:<port>` through a reserved [config key](../commands.md#unified-config-keys), `metering.base_url`, which the adapter maps into whatever the agent reads for its OpenAI-compatible endpoint, typically an environment variable such as `OPENAI_BASE_URL`. The operator points `metering.upstream_base_url` at the real provider. The agent believes it is talking to its provider. It is talking to the meter.

The listener is a transparent forward proxy. It forwards the method, path, query, and headers verbatim, including the agent's own `Authorization` header, so the agent's key flows upstream untouched. It relays the status, headers, and body back unchanged; an unreachable upstream gets an honest `502`. On the way back it reads the body once, skims `usage.prompt_tokens` and `usage.completion_tokens`, and pushes those two integers onto a queue. Nothing else leaves the proxy: no body, header, URL, or key reaches a log, an error, or a ledger row, and a sentinel-key test proves it.

Both sources end in the same place. The supervisor drains the queue on its reaper tick and hands each count to `ingest_usage`, the one function allowed to write the Usage Ledger: an append-only `usage_events` table in the engine's SQLite database, one committed transaction per event. Each row carries the instance, the Run id, both token counts, the metering source, a timestamp, and a sequence number, under a unique index so a replayed event is a no-op. A Run spans one `starting` transition to the next terminal state; per-run totals cover that span, and cumulative totals sum every row the instance ever wrote. An observed agent supplies no sequence, so the engine mints one per completion. If the ledger store fails mid-run, the un-committed minted events park in memory and retry as-is on the following ticks — never re-minted, so the dedup keys stay stable; the retry is bounded at 3 consecutive failed passes at the same front event (roughly 750 milliseconds at the ~250 ms reaper cadence), after which the poisoned event is skipped with a loud diagnostic while the rest keep counting, and the pending buffer itself is capped at 1,024 events so a long outage cannot grow engine memory without bound — when the cap bites, the oldest parked events are dropped, loudly. A failure during the terminal drain (stop or crash-reap) is announced as lost, never parked-for-retry, because there is no next pass to keep that promise.

Budgets are ordinary config keys, changeable while the agent runs:

```bash
hemaka agent config set my-agent budget.tokens.cumulative 500000
hemaka agent config set my-agent budget.breach_action pause
hemaka agent config set my-agent cost.rate.input 3.00
hemaka agent config set my-agent cost.rate.output 15.00
hemaka agent config set my-agent budget.dollars.cumulative 10.00
```

Where enforcement runs matters most to me. Immediately after a fresh row commits, in the same synchronous call, `ingest_usage` re-reads the current budget from config, reads the just-committed per-run and cumulative totals, and runs a pure evaluator over them. Tokens are checked first, per-run before cumulative, at a `>=` threshold: reaching the ceiling is the breach. Then, if a rate exists, the dollar cap is checked the same way. On a breach the supervisor writes a breach event to a durable per-instance log before anything else, then executes the action: `pause`, `stop`, or `warn`. Pause is the default, and a breach fires once per dimension and scope per Run.

The obvious alternative deserves a fair hearing. A separate watcher that reads the ledger every second and pauses anything over its ceiling is simpler, and it keeps ingestion fast and dumb. I rejected it because it opens a window between "usage recorded" and "budget checked" in which the total is over the line and nobody has acted. Putting the evaluator inside the commit path closes the window by construction. The price is a config read and two comparisons on every ingested event. I will pay that.

Money gets the same suspicion. Rates are dollars per million tokens, stored as integer micro-dollars, never floats. Each row is priced at the rate in force when it committed, so changing the rate never rewrites history. Every dollar figure Hemaka renders comes out of one module and carries the label `estimated`, and a CI lint fails the build if any other module formats a dollar. Running `hemaka agent usage my-agent --json` prints the ledger's view of that instance:

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

Those token totals are the ledger sums exactly, the same numbers `hemaka agent list` and `hemaka agent show` print. With no rate configured the dollar fields are absent rather than `0`. The code is [source-available](https://github.com/Ktesio/ktesio), so every sentence in this section can be checked against it.

## What the meter can't see

A meter is only as good as the wire it sits on, and today the wire has gaps. I would rather list them here than have you find them on an invoice.

The proxy sees only traffic the agent sends to the injected address. If the adapter maps `metering.base_url` nowhere, if the agent ignores the environment variable, or if it calls a second endpoint, an embeddings API, or a tool with its own model client, none of that is metered and nothing warns you. The totals simply stay low, and a low total looks like good behavior.

An `https://` upstream works: the proxy dials it directly over a vendored rustls+ring stack — no system TLS library, the same vendored philosophy as the bundled SQLite, and one rustls+ring pair shared with `hemaka`'s own download client. The trust roots are vendored Mozilla roots (webpki-roots), not your OS keychain, so a provider behind a private CA your machine trusts will still fail the handshake with an honest 502.

Streaming completions are metered. When the agent asks to `"stream": true` on a chat-completions call, the proxy adds one thing to the forwarded request — `stream_options.include_usage` — so the provider emits the terminal usage frame (this is the one deliberate, upstream-visible modification of the agent's request; if the agent set its own `stream_options`, its choice wins), then reads the terminal server-sent event frame and meters it exactly once. The modification is scoped to chat-completions paths on purpose: only there does the parser understand the response, and other streaming endpoints would reject the injected `stream_options` with a 400 — metering must never break the call it meters, so any other streaming request is forwarded byte-for-byte. Scope honesty: on those other paths usage is not *requested* — not *unmeterable*. The response parse routes on content-type, not path, so if a provider sends a usage frame unprompted on any metered response, the proxy records it all the same. A response with no `usage` object, a malformed one, a stream with no terminal frame, or a provider whose usage schema is not OpenAI-shaped still skips silently: the call succeeds and the ledger misses it. The relay is store-and-forward — whole bodies are buffered before they move, so a completion's round-trip includes that buffering pass, and a body over the 64 MiB cap fails the relay with an honest 502 instead of buffering unboundedly. I accept both costs because a faithful relay plus one bounded parse is a small amount of code I can test exhaustively. So read an observed instance's totals as a lower bound.

*Update 2026-09-15 (AI-47 ratified, epic-12): both gaps above shipped in the epic-12 batch — the streaming usage parse (story 12-2, the terminal `include_usage` SSE frame) and the HTTPS upstream via vendored rustls+ring (story 12-3). The non-OpenAI usage-schema deferral remains, behind the named parse seam.*

The parser reads two fields and ignores the rest. Cached input tokens, reasoning tokens, and provider pricing tiers are not modeled, so the dollar figure is an estimate from a flat per-direction rate and will not match the provider's invoice. A `reconciled` label exists in the type, but no code produces it today.

Enforcement acts on committed rows, and rows commit on a 250 millisecond reaper tick. Between the response that crossed the ceiling and the pause, the agent may have more requests in flight, and those reach the provider. A pause is only as strong as the adapter's declaration: `guaranteed` on Unix is a real signal stop of the whole process group, while `best-effort` records the state change with a visible qualifier and sends nothing to the process, which may keep running. The native Hermes adapter declares best-effort everywhere, because that gateway has no freeze mechanism. `warn` records the breach and does nothing else. A budget is a ceiling on what the ledger has seen, not a fence around what the agent can spend.

The loopback bind guards against accidental network exposure of the agent's traffic and key. It is not a sandbox, and an Agent Home is process and filesystem isolation, nothing stronger.

Finally, the listener lives with the engine. A standalone `hemaka agent start` supervises the process only for that command's lifetime, and the listener dies with it. If the engine crashes and a later open re-adopts a surviving observed agent, that agent's `base_url` still points at a dead port, and its model calls fail with a connection error until you stop and start it. That fails loud, never wrong, but it is a gap, and the supervising daemon that would close it is still on the roadmap. Self-reported instances have no listener and are unaffected, which is an irony I have not fixed yet. For the same lifetime reason, `hemaka agent start --detach` refuses an observed instance outright before anything changes: a detached start would inject an address whose listener dies with the starting command.

An observed agent's numbers are numbers it could not have faked, and they are a lower bound on what it spent.

Run agents like services. Give them a supervisor, a ledger you did not ask them to fill in, and a ceiling that pauses them before the invoice does. That is all Hemaka is trying to be, and where it falls short today, it says so.
