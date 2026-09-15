---
type: product-decision-proposal
date: 2026-09-15
author: ZCode (operating the architect seat) per AI-58 draft-then-ratify, for Islam
status: awaiting-ratification  # touches ZERO real planning artifacts; applied only after Islam picks from the bounded options
decides: "AI-20 (daemon/detach for durable cross-CLI supervision) + AI-47 (engine-observed production gaps: TLS, streaming parse, provider schemas) + the stranded observed-channel defer (epic-11 retro F3)"
inputDocuments:
  - _bmad-output/implementation-artifacts/sprint-status.yaml (AI-20, AI-47 entries)
  - docs/architecture.md:54 (AD-7 engine-observed metering, documented v1 deferrals + adoption strand)
  - docs/design/metering-agents-you-dont-control.md:84-86 (first-person rationale for both deferrals)
  - _bmad-output/implementation-artifacts/deferred-work.md (spec-11-1 stranded defer: observed drain lossiness under store failure)
  - crates/ktesio-engine/src/metering/listener.rs (upstream refusal path, loopback bind)
  - Cargo.toml (rusqlite bundled — the existing C-compiling precedent)
---

# AI-20 + AI-47 — the two open product calls (epic-11 leftovers)

**2026-09-15 · grounded in the shipped code · nothing applied until you pick**

---

## Part A — AI-20: durable cross-CLI supervision

### The situation (as shipped)

`kt agent start` does NOT keep an agent alive across separate CLI commands. The
single-lifetime supervisor SIGKILLs the process group when its handle drops
(backend `Drop`), so a clean CLI exit kills the agent. Adoption
(AD-5 fingerprints + the PID-reuse guard, hardened by 11-1/11-5) rescues only
the engine-CRASH case, never a clean exit. The gap is surfaced honestly today
(a stderr notice on `start`) — but the product is named "runs agents like
services", and this is the widest gap between that claim and the behavior.

### The options

**(a) Engine-as-daemon/service.** A long-lived `kt daemon` (or OS service)
owns the supervisor; every CLI command becomes a client over IPC (UDS /
named pipe). True durable supervision: crash detection, budget enforcement,
and event delivery run continuously; the observed-metering listener ownership
problem (below) dissolves; embedding hosts already get this shape for free.
Costs: a new always-on component (its own lifecycle, crash recovery, upgrade
story), a NEW public IPC surface to version and freeze like the Adapter
Contract, socket security/multi-user questions, and the largest build of the
three — one focused epic minimum.

**(b) `kt agent start --detach`.** On a detached start, the engine persists
`running` and deliberately drops the handle WITHOUT the kill — the process
group survives the CLI exit, and the next command adopts it through the
existing, already-hardened fingerprint path. Small diff on machinery 11-1 and
11-5 just finished proving (fail-closed spawn fingerprints, Windows-positive
adoption tests). Costs: supervision is command-scoped — between commands
there is NO crash detection, NO budget enforcement, NO event delivery; those
enforcement windows must be documented honestly in the command's `--help` and
the stderr notice. And it does NOT solve the observed-metering strand: a
detached engine-observed instance's loopback listener still dies with the
command (same broken-`base_url` loud failure as today's adoption strand), so
detach v1 should refuse + explain for engine-observed instances rather than
detach them into a broken state.

**(c) Document single-lifetime as intended v1.** Zero build cost; fully
honest; leaves the core product gap open — every future "why did my agent
die when I closed the terminal" lands on it. This is what ships today
regardless, until (b) or (a) exists.

### Recommendation

**(b) now — one story — with (a) as a deliberate future epic when embedder
demand or the enforcement-window pain materializes.** The asymmetry: (b)
reuses machinery this project just invested two epics hardening, and closes
the surprise that actually hurts users (agent dies on CLI exit); (a) is the
true end-state but buys durability the embedding story already delivers to
hosts, at the price of a versioned IPC surface this early. (c) remains the
truth for non-detached starts either way. Ratify B by also accepting its
documented enforcement windows; if those windows are unacceptable for your
use cases, the honest answer is (a), not (b).

---

## Part B — AI-47: making the engine-observed channel production-usable

### (a) The HTTPS upstream — the TLS dependency call

The design doc's own words: *"I shipped without a TLS stack to keep the
dependency tree small and free of a C build … to meter an agent that talks
to a hosted provider you need a local hop that speaks plain HTTP to Ktesio
and TLS to the provider."* One fact reframes the trade: **the "no C build"
ship has sailed** — `rusqlite` bundled already compiles C (libsqlite3-sys)
on every fresh build, and the MSRV floor exists because of it. The real
philosophy is "no SYSTEM library dependencies; vendored builds OK."

- **(i) rustls + ring (recommended).** The Rust-mainline TLS client; vendored,
  self-contained, identical behavior on Linux/macOS/Windows (no
  system-OpenSQL variance). Costs: a real dependency-tree increase (compile
  time, audit surface — new supply-chain teeth for the CI boundary review),
  and ring is C+asm (a C compiler is already required today, so this adds
  degree, not kind). The `https://`-refused-at-start error becomes a working
  forward.
- **(ii) Keep HTTP-only; document the local-hop recipe.** Zero new deps; the
  operator runs one TLS-terminating proxy (caddy/nginx) locally. Honest, and
  the docs already say this — but it makes "meter any real provider" a
  DIY exercise for every user, forever.
- **(iii) native-tls (system TLS: Schannel / Security.framework / OpenSSL).**
  No vendored C on win/mac (system frameworks), but OpenSSL variance on Linux
  is exactly the system-dependency footprint the philosophy forbids, and the
  API is the least uniform. Not recommended.

### (b) Streaming-response usage parse — the scope call

Today a `stream: true` completion is relayed faithfully and metered as ZERO —
the channel's core promise ("governance never depends on the agent's
cooperation") silently fails for a large share of real usage. The design doc
deferred it as "a streaming state machine is not [exhaustively testable]" —
but the deferral overstated it: the OpenAI contract is ONE well-defined shape
(a terminal SSE frame carrying `usage` when `stream_options.include_usage`
is requested), the forward proxy already sees every byte, and an SSE
line-scanner is O(1) memory and exactly the kind of small machine this repo
tests exhaustively (the `observed_metering.rs` std-only upstream stub gains
an SSE case). **Recommendation: YES — implement it**, requesting
`include_usage` on forwarded stream requests and parsing the terminal frame
into the same `ParsedUsage` choke point. Also meter the RELAYED-request
accounting honestly (the injected `stream_options` is an upstream-visible
modification — document it).

### (c) Non-OpenAI provider usage schemas

Defer behind a named provider-parse seam (`OpenAiUsage` today; a
`ProviderUsageParse` trait later). Document per-provider honesty in the
design doc. No action this cycle.

### Adjacent — fold in: the stranded observed-drain defer (epic-11 retro F3)

11-1's review deferred "the observed channel is lossy under store failure
(best-effort drain, no cursor, no retry) — needs a product/ops call" → 11-6,
which closed without it. It belongs to THIS decision space: give the observed
drain the same AI-41 treatment the self-reported channel got (park, bounded
retry, loud skip). Recommended as the third item of the same epic.

### Sequencing recommendation

**(b) streaming parse first** — it delivers working metering to streaming
agents on TODAY's HTTP path (including through a local TLS hop); **then (a)
rustls** removes the local-hop requirement; **then the drain-durability
item**. Each is one story-sized; the three together make the observed
channel actually production-usable, which is AI-47's stated goal.

---

## Ratification — pick one per line (apply verbatim per AI-58)

1. **AI-20:** (a) engine-as-daemon epic · (b) `start --detach` story · (c) document v1 as-is — **recommended: (b)**
2. **AI-47(a) TLS:** (i) rustls+ring · (ii) HTTP-only + local-hop recipe · (iii) native-tls — **recommended: (i), sequenced after the streaming parse**
3. **AI-47(b) streaming usage parse:** yes / no / defer — **recommended: yes, first**
4. **AI-47(c) provider schemas:** defer behind a parse seam — **recommended: defer**
5. **Observed-drain durability (retro F3):** fold into the AI-47 epic / keep deferred — **recommended: fold**

On ratification the applied set lands as: epics.md + sprint tracker updates,
the AD-7 v1-deferral paragraphs in `docs/architecture.md` + the design doc
rewritten to the ratified reality, `deferred-work.md` markers, and a
commissioned story batch in the ratified order.
