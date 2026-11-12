---
title: "Run Keys and key custody"
description: "Each Run's own credential: issued per launch, shown once, stored as a hash, revoked on exit, end or demand. Key custody, Scent-Check and bypass detection are planned."
---

# Run Keys and key custody

> **On main, not yet released.** Run Keys ship with the next release. Key custody, Scent-Check Modes, bypass detection and the authenticated proxy beyond loopback are still planned; see [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/run-keys/](https://hekma.ktesio.dev/docs/run-keys/), where it is kept current.

A **Run Key** is a [Run](runs.md)'s own credential. Whoever holds it can act on that Run and the child Runs beneath it as the Run's **Caller**: open a child Run, close a child, and tighten limits. It cannot read the fleet, change configuration, set Policies, raise a limit, or touch any other Run.

A Run Key is not yet a replacement for your provider key: today the agent still holds the provider key you configured. Key custody, which puts the Run Key in the agent's API-key slot and keeps the real key inside Hekma, is planned (below).

## What works today

| Area | What you get now |
|---|---|
| A key per launch | Every supervised start, and every Restart Policy relaunch, issues a fresh Run Key and hands it to the agent in `HEKMA_RUN_KEY`. |
| A pair per child Run | `POST /v0/runs` (and `hekma run open --print-key`) returns the child's Run Key and a refresh credential, once. |
| Hash-only storage | Hekma stores the SHA-256 of each credential, never the value. A short **token id** (12 hex characters) labels it in records and reports. |
| Revocation | A key is revoked when its process exits, when its Run ends, after a refresh, when it shows up in a URL, and on demand with `hekma run revoke-keys`. The next request presenting it is refused. |
| Refresh with reuse detection | A refresh credential trades itself once for a new pair. Presenting a used one again revokes the Run's current pair and raises the `credential-reuse` Anomaly. |
| Secret references | `secret:NAME` keeps a provider key out of Hekma's config and output, but the agent receives the real key. See [Secrets](https://hekma.ktesio.dev/docs/config-secrets-memory/#secrets). |

### The format

```text
hk_run_<52 characters>   a Run Key
hk_ref_<52 characters>   a refresh credential
hk_ann_<52 characters>   an announce credential (format only; nothing issues one yet)
```

Each is 32 bytes from the operating system's random source, written in Crockford base32. If the random source fails, nothing is issued and nothing changes: a start is refused before the agent is spawned, an open or refresh before anything is written. There is no fallback.

### Shown once

The value exists in three places only: the environment of the agent it was issued to, the `201` of the child open that asked for it, and the `200` of a refresh. No other route, event, SSE frame, export row, `--json` document or log line carries it, and nothing can show it again. A lost Run Key is replaced by a relaunch (a supervised agent) or a refresh, never re-read.

```bash
hekma run open --parent run_01JZ6Q9G3S8V2X7K4M1N5P0R2T --print-key
```

Without `--print-key`, `hekma run open` issues no credential at all, so nothing lands in your terminal scrollback or CI logs. With it, the Run Key and refresh credential are printed once, with a note on stderr:

```text
These credentials are shown once and are not stored. Keep them somewhere safe; `hekma run revoke-keys` revokes them.
```

### A supervised agent's key

Every supervised agent gets its launch's Run Key in the `HEKMA_RUN_KEY` environment variable, so the agent and any in-process helper can open and close child Runs under it and tighten their limits through the [Control API](control-api.md#run-keys) on loopback. An Adapter may also deliver it to its own environment variable by mapping the reserved `run.key` key with an `env` target; a `flag` or `file` target is refused at registration. See [the manifest's reserved keys](manifest.md#reserved-credential-keys).

- **A key per launch.** A Restart Policy relaunch keeps the Run, its id, budgets and labels, and gets a new key. The previous launch's key is revoked when its process is confirmed exited, before the relaunch starts. An engine restart that adopts a live agent changes nothing: the agent keeps its key.
- **Loopback only.** A supervised agent's key has no network use, so it is refused on a `--listen` listener that is not loopback. Keys of Runs opened over the Control API are accepted there.
- **Never in a file or an argument.** The key reaches only the agent's environment. `effective-config.json` and `hekma agent config get` never show it, and setting `run.key` or `run.refresh_key` in configuration is refused.
- **Children inherit it.** Processes the agent starts inherit its environment, `HEKMA_RUN_KEY` included. That is expected: they act within the same Run.
- **A short window at start.** The key is recognised once the launch has been recorded, a few hundred milliseconds after spawn. A request in that window is refused as unknown; retry it.
- **No refresh credential by default.** A supervised agent gets one only if its Adapter maps `run.refresh_key` to an `env` target. Its rotation is the relaunch.
- **Runs that started before Run Keys.** An agent running when you upgrade keeps running without a key and gets one at its next launch. `hekma run keys` says so.

### No clock expiry

A Run Key does not expire on a timer: agents do not refresh an API key mid-run, and a supervised Run can last months. It is bounded instead by its process (revoked at exit), its Run (revoked at end), a refresh, an Operator revocation and URL exposure. The Run's budgets bound what it can spend.

### Refresh

```bash
curl -s -X POST -H "Authorization: Bearer $REFRESH_CREDENTIAL" http://127.0.0.1:<port>/v0/runs/refresh
```

A refresh retires the presented credential, revokes its Run Key at once, and returns a new Run Key and refresh credential for the same Run. Possession is the binding: Hekma does not check which process presents it. A used refresh credential presented again revokes the Run's current pair, raises the `credential-reuse` Anomaly (listed by `hekma ps`), and answers `401 credential-revoked`, so a silent takeover becomes a visible one. `hekma run revoke-keys` on the Run acknowledges and clears the Anomaly.

### Revocation

```bash
hekma run keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T          # token ids, classes, states; never a key
hekma run revoke-keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T   # and every key issued under them
hekma run revoke-keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T --no-cascade
```

A revocation is committed and published before the command returns, so the very next request presenting the key is refused, and a hard kill of the Service Host cannot undo it. By default it cascades: keys of child Runs opened with the revoked keys, and their refresh chains, are revoked too. Revocation never ends a Run; children opened under a revoked key stay open.

A Run that ends revokes its own keys; its child Runs that outlive it keep theirs.

### Never in a URL

Credentials go in the `Authorization` header. A request whose path or query carries one, raw or percent-encoded, is refused with `400 credential-in-url` before it is authenticated, and the exposed credential is revoked: it has reached someone's access log. The per-agent metering listener applies the same rule (a typed `hekma_credential_in_url` error, nothing forwarded upstream), and strips any `x-hekma-run-key` header before forwarding a call to the provider.

## What is coming

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

The default Policy starts at `off`, and `enforce` is never a default. Only `off` is accepted today. The mode governs identity only and never changes what a [Breach Action](https://hekma.ktesio.dev/docs/concepts/#breach-action) does. A caller cannot lower it.

### The shared proxy reading Run Keys

A shared Metering Proxy will read the Run Key on each call, find the Run, apply its Policy and forward, refusing a revoked key from its next call. Calls held at the proxy will be re-checked when they are released.

### Bypass detection

The small supervisor that will own each supervised process (the Shim) will watch its process tree for direct connections to known provider hosts. A bypass will show as an Anomaly naming the Run and host, and as an OTLP counter; it never stops the Run. Where an OS gives no reliable signal, detection is reported as "unavailable", never as clean.

### An authenticated proxy beyond loopback

A proxy reachable from other machines will require TLS and a credential, and will not start without them. Clients that can only change their base URL will get a proxy-access credential per endpoint binding, shown once and stored as a hash; it is not a Run's identity for Scent-Check.

## What it does not promise

- A process running as your user can read the agent's environment, and could already act as the Operator through `control.json`. `state.db` holds only hashes, so a copy of it reveals no key.
- An agent that prints its own environment to its output log puts its key there. Revoke it with `hekma run revoke-keys`.
- Possession is the only binding: whoever holds a Run Key or a refresh credential can use it.
- Key custody keeps the provider key away from the agent's process and files once it ships. It does not contain a compromised agent. Hekma is not a sandbox, and key custody will get a security review before these docs describe it as supported.

## Open questions

- **The default Scent-Check Mode per environment.** The proposal is `off` on a laptop, `shadow` under a long-running host, and `enforce` only by choice. Until it is decided, the default Policy starts at `off`, and the decision will never overwrite a default Policy you have edited.

## Read next

- [Runs](runs.md): what a Run is and the Control Depth ladder.
- [Control API](control-api.md#run-keys): the credential classes, the routes and the error codes.
- [Commands](commands.md#run-keys): `hekma run keys` and `hekma run revoke-keys`.
- [Policies](policies.md): Selectors, limits and the default Policy.
- [Meter token usage](https://hekma.ktesio.dev/docs/metering/): the metering that works today.
