---
title: "Run Keys and key custody"
description: "How Hekma will give each Run its own credential in place of your provider key, keep the real key itself, and flag calls that go around it. Planned."
---

# Run Keys and key custody

> **Planned.** Nothing on this page is in Hekma 0.11.0 or on main yet. It describes a design being built in the open, and names, commands and defaults may change before it ships. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/run-keys/](https://hekma.ktesio.dev/docs/run-keys/), where it is kept current.

Today an agent that Hekma meters still holds your real provider key. If it calls the provider directly, that call is neither counted nor governed, and Hekma cannot tell. A **Run Key** will close that gap: one credential per [Run](runs.md) that the agent uses as its provider API key, while Hekma keeps the real key and swaps it in on the way out.

Run many coding agents yourself, and every session's calls will pass through Hekma. Run CrewAI or LangGraph flows in production, and each call will carry its Run's identity and labels, so Policies and alerts follow the Run.

## What works today

| Area | What you get now |
|---|---|
| Secret references | `secret:NAME` keeps a key out of Hekma's config and output, but the agent receives the real key. See [Secrets](https://hekma.ktesio.dev/docs/config-secrets-memory/#secrets). |
| Engine-observed metering | A per-agent proxy on `127.0.0.1` reads token counts from the wire and never logs keys. See [Engine-observed usage](https://hekma.ktesio.dev/docs/metering/#engine-observed). |

Nothing stops an agent reaching its provider around the proxy with the key it was given, and nothing reports it.

## What is coming

### Run Keys

A Run Key will be issued when a supervised Run starts, or when a Run announces itself through the **Metering Proxy** (Hekma's interception point for model traffic) or the [Control API](https://hekma.ktesio.dev/docs/control-api/). It will be:

- opaque, shown once to its holder and stored only as a hash;
- valid for the Run's life, with no clock expiry, since agents do not refresh an API key mid-run;
- revocable at any time, refused from its next call;
- delivered to a supervised agent only through an environment variable, and never put in a URL;
- rotatable through a separate refresh credential that only the original process holds.

### Key custody

When you configure a supervised agent's provider key through Hekma, custody will be the default. Hekma will put the Run Key in the agent's API-key slot and the proxy's address in its base URL. The real key will never appear in the agent's environment, arguments or [Agent Home](https://hekma.ktesio.dev/docs/concepts/#agent-home), and the proxy will send it only to its configured upstream origin, over HTTPS.

An agent that brings its own key cannot be identified on the wire. A self-reported one will show at **Control Depth** (how far Hekma controls a Run) `supervised (no wire)`, metered from its own reports and without wire controls; an engine-observed one will be refused at start.

### Scent-Check Modes

A **Scent-Check Mode**, set per [Policy](policies.md), will decide what happens to a call without a valid Run Key:

| Mode | An unscented call is |
|---|---|
| `off` | Not checked. |
| `shadow` | Served and recorded as an **Anomaly**: listed and counted in OTLP, never acting on the Run. |
| `enforce` | Refused with a typed response. |

The default Policy starts at `off`, and `enforce` is never a default. The mode governs identity only and never changes what a [Breach Action](https://hekma.ktesio.dev/docs/concepts/#breach-action) does. A caller cannot lower it.

### Bypass detection

The small supervisor that will own each supervised process (the Shim) will watch its process tree for direct connections to known provider hosts. A bypass will show as an Anomaly naming the Run and host, and as an OTLP counter; it never stops the Run. Where an OS gives no reliable signal, detection is reported as "unavailable", never as clean.

### An authenticated proxy beyond loopback

A proxy reachable from other machines will require TLS and a credential, and will not start without them. Clients that can only change their base URL will get a proxy-access credential per endpoint binding, shown once and stored as a hash; it is not a Run's identity for Scent-Check.

## How it will work

On each call, the proxy will read the Run Key, find the Run, apply its Policy, swap in the real key and forward.

A sketch of what a supervised agent will see under key custody; the final names may differ:

```text
OPENAI_BASE_URL=http://127.0.0.1:<port>
OPENAI_API_KEY=hk_run_…    # its Run Key, not your provider key
```

A sketch of the planned command to try Scent-Check before enforcing anything; the final flags may differ:

```text
hekma policy set default --scent-check shadow
```

## What it does not promise

Key custody keeps the provider key away from the agent's process and files. It does not contain a compromised agent: a process running as your user can still read the secrets file, and one using other credentials is caught only by bypass detection, best-effort. Hekma is not a sandbox, and key custody will get a security review before these docs describe it as supported.

## Open questions

- **The default Scent-Check Mode per environment.** The proposal is `off` on a laptop, `shadow` under a long-running host, and `enforce` only by choice. Until it is decided, the default Policy starts at `off`, and the decision will never overwrite a default Policy you have edited.

## Read next

- [Runs](runs.md): what a Run is and the Control Depth ladder.
- [Policies](policies.md): Selectors, limits and the default Policy.
- [Attach any agent](attach.md): Runs Hekma did not start.
- [Meter token usage](https://hekma.ktesio.dev/docs/metering/): the metering that works today.
