---
title: "Run Keys and key custody"
description: "Each Run's own credential: issued per launch, shown once, stored as a hash, revoked on exit, end or demand. Key custody is in preview; Scent-Check Modes act at the metering proxy; bypass detection is a preview for agents in their own Shim."
---

# Run Keys and key custody

> **On main, not yet released.** Run Keys ship with the next release. Key custody is on main as a preview, not yet supported. Scent-Check Modes are on main too. The authenticated proxy beyond loopback is on main too. Bypass detection is on main as a preview, for agents in their own Shim; see [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/run-keys/](https://hekma.ktesio.dev/docs/run-keys/), where it is kept current.

A **Run Key** is a [Run](runs.md)'s own credential. Whoever holds it can act on that Run and the child Runs beneath it as the Run's **Caller**: open a child Run, close a child, and tighten limits. It cannot read the fleet, change configuration, set Policies, raise a limit, or touch any other Run.

With [key custody](#key-custody-preview--not-yet-supported) (a preview), a Run Key replaces your provider key: Hekma puts the Run Key in the agent's API-key slot and keeps the real key inside Hekma. Without it, the agent holds the provider key you configured.

## What works on main

| Area | What you get now |
|---|---|
| A key per launch | Every supervised start, and every Restart Policy relaunch, issues a fresh Run Key and hands it to the agent in `HEKMA_RUN_KEY`. |
| A pair per child Run | `POST /v0/runs` (and `hekma run open --print-key`) returns the child's Run Key and a refresh credential, once. |
| A pair per announced Run | An [announce](announce.md) that creates its Run returns the Run's Run Key and refresh credential once: in the `201` of `POST /v0/runs/announce`, or in the `x-hekma-run-key` and `x-hekma-refresh` response headers of the announcing call. Announcing an open identity again issues nothing. A Run that [inbound telemetry](telemetry.md#inbound-telemetry-v1metrics) created holds no Run Key, ever. |
| Hash-only storage | Hekma stores the SHA-256 of each credential, never the value. A short **token id** (12 hex characters) labels it in records and reports. |
| Revocation | A key is revoked when its process exits, when its Run ends, after a refresh, when it shows up in a URL, and on demand with `hekma run revoke-keys`. The next request presenting it is refused. |
| Refresh with reuse detection | A refresh credential trades itself once for a new pair. Presenting a used one again — or seeing one in a URL — revokes the Run's current pair and raises the `credential-reuse` Anomaly. |
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

- **A key per launch.** A Restart Policy relaunch keeps the Run, its id, budgets and labels, and gets a new key. The previous launch's key is revoked when its process is confirmed exited, before the relaunch starts. Recording the exit and revoking the key are one write: if the revocation cannot be written, the exit is not recorded either. Issuing a launch's key also revokes anything an earlier launch of the Run still holds, so only one launch's key is ever live. An engine restart that adopts a live agent changes nothing: the agent keeps its key.
- **Loopback only.** A supervised agent's key, and its refresh credential if it has one, have no network use, so they are refused on a `--listen` listener that is not loopback. A listener bound to `0.0.0.0` counts as beyond loopback. Keys of Runs opened over the Control API are accepted there.
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

A refresh retires the presented credential, revokes its Run Key at once, and returns a new Run Key and refresh credential for the same Run. Possession is the binding: Hekma does not check which process presents it. A used refresh credential presented again revokes the current pair of its own launch (for a Run opened over the Control API, the Run's current pair), raises the `credential-reuse` Anomaly (listed by `hekma ps`), and answers `401 credential-revoked`, so a silent takeover becomes a visible one. It also revokes every used refresh credential of the Run. A credential from an earlier launch never revokes a later launch's key. A used refresh credential also leaves service wherever the credentials it belonged to do: at its launch's exit, at its Run's end, and under a `revoke-keys` cascade. Seen in a URL it is reuse evidence — only a holder of the value can put it there — and takes the same response as presenting it: the current pair of its own launch is revoked, every used refresh credential of the Run is revoked, and the `credential-reuse` Anomaly is raised, so one unauthenticated request cannot quietly disarm the detection. `hekma run revoke-keys` on the Run acknowledges and clears the Anomaly, and revokes the Run's used refresh credentials too, so an old one found later in a log is a plain `401 credential-revoked` that changes nothing.

### Revocation

```bash
hekma run keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T          # token ids, classes, states; never a key
hekma run revoke-keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T   # and every key issued under them
hekma run revoke-keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T --no-cascade
```

A revocation is committed and published before the command returns, so the very next request presenting the key is refused, and a hard kill of the Service Host cannot undo it. Like every write to `state.db`, it is not forced to disk on each commit: an operating-system crash or a power loss can lose the last moment of writes, a revocation included. After one, check with `hekma run keys` and revoke again if needed. By default it cascades: keys of child Runs opened with the revoked keys, and their refresh chains, are revoked too. Revocation never ends a Run; children opened under a revoked key stay open.

A call held at the metering proxy is checked again when it is released: if its Run Key was revoked while it waited, it is refused with `401 hekma_credential_revoked`, never forwarded.

A Run that ends revokes its own keys; its child Runs that outlive it keep theirs. When a supervised agent's process exits and its Run ends, the in-process child Runs that end with it lose their keys in the same write as the exit. Removing an instance (`hekma rm --force`) removes its Runs and their keys with it.

### Never in a URL

Credentials go in the `Authorization` header. A request whose path or query carries one, raw or percent-encoded, is refused with `400 credential-in-url` before it is authenticated, and the exposed credential is revoked: it has reached someone's access log. An active credential Hekma still honours is revoked. A used refresh credential is reuse evidence — only a holder of the value can put it in a URL — so it takes the same response as presenting it: the current pair of its own launch is revoked, every used refresh credential of the Run is revoked, and the `credential-reuse` Anomaly is raised. A value that was never issued, or is already revoked, is just refused. The scan keeps at most 16 distinct tokens per URL; a request carrying more is refused all the same, and a rate-limited diagnostic says how many could not be revoked. The [metering proxy](metering-proxy.md) refuses a credential in a URL too, with a typed `400 hekma_credential_in_url`, and forwards nothing. It does not revoke the credential, because anyone on loopback could have sent that request, and revoking would let any local process cancel any Run's key.

### On the wire

The [metering proxy](metering-proxy.md) reads a Run Key sent in `x-hekma-run-key` on every call, and charges the call to the key's Run:
- An unknown key gets `401 hekma_unknown_credential`.
- A key revoked in the last 24 hours gets `401 hekma_credential_revoked`, from its very next call.
- On a supervised agent's Run route, the key's Run must be that Run or beneath it, else the call gets `403 hekma_run_out_of_scope`. This is how a helper inside an agent charges its own child Run.
- A key never chooses where a call goes: the upstream is the route's or the binding's.

A Run Key in the API-key slot (`authorization`, `x-api-key`, `x-goog-api-key` or `api-key`) asks for the custodied key. If its Run holds none, or the key was not issued to a supervised agent or a credentialed open, the call is refused with `400 hekma_custody_unavailable` rather than forwarded to the provider. Without custody, send the provider key in `authorization` and the Run Key in `x-hekma-run-key`. Every `x-hekma-*` header is removed before the call is forwarded.

### Heartbeat and close

A Run Key heartbeats its own Run or one beneath it with `POST /v0/runs/{id}/heartbeat`, renewing the Run's [Lease](announce.md#leases-and-the-windows), and closes a child as before. A binding's **announce credential** (`hk_ann_`) announces Runs beneath its binding, and heartbeats or closes only the Runs it created; on any other route, or for another Run, it is refused with `403`.

### External ids

A child open's `external_id` makes a retry safe: the same id returns the same Run. Ids are scoped by who opens. The Operator and Hosts share one namespace; each Run's keys have their own, so a Run Key can neither take over nor discover an id the Operator or a Host uses.

## Key custody (preview — not yet supported)

> **Preview.** Key custody is on Hekma's main branch, but it is not yet supported: it becomes supported when the pending security rulings are recorded and its test criteria pass in CI. Until then, treat it as a preview whose behaviour may change.

With key custody, Hekma holds your provider key and the agent never sees it. The agent gets a Run Key in its API-key slot and the metering proxy's address as its base URL. When a call arrives, the proxy checks the Run Key, swaps in your real key, and sends the call to the provider.

### Turning it on

Set the agent's provider key as a secret reference:

```sh
hekma agent config set my-agent provider.api_key secret:OPENAI_KEY
```

Custody is then on by default. The adapter must map `provider.api_key` to an environment variable (an `env` target in its manifest); a `flag` or `file` target is refused, because it would put the key on the command line or in the Agent Home.

- **`provider.api_key`**, `secret:NAME` only: the provider key Hekma holds. Any other value is refused.
- **`provider.custody`**, `on` (the default) or `off`: `off` delivers the real key to the agent as before, while Hekma still holds the reference.
- **`provider.isolate_home`**, `off` (the default) or `on`: points the agent's home-directory variables at a directory inside its Agent Home (see the limits below).

At start, under custody, Hekma:

- resolves the key and refuses the start if it does not resolve;
- puts this launch's Run Key in the agent's API-key variable, and never the real key;
- clears the inherited environment (only the agent's declared variables and the OS basics pass), so a provider key in the environment of `hekma serve` never reaches the agent;
- refuses the start when the same key would also reach the agent another way: another config leaf naming the same secret, an `agent.<VAR>` pass-through naming a provider variable, or a provider variable in the manifest's launch environment. The error names the other delivery.

The provider variables Hekma recognises (list version 1) are `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `MISTRAL_API_KEY`, `GROQ_API_KEY`, `DEEPSEEK_API_KEY`, `OPENROUTER_API_KEY`, `AZURE_OPENAI_API_KEY`, `XAI_API_KEY` and `HF_TOKEN`.

### Where the key may go

The proxy sends a custodied key only over HTTPS, with certificate checks, to an origin pinned beside the key, and only to paths on the key's list. You can declare both in your secrets file, next to the key:

```toml
OPENAI_KEY = "sk-..."

[custody.OPENAI_KEY]
origins = ["https://api.openai.com"]
paths = ["/v1/chat/completions", "/v1/responses"]
```

Without a `[custody.NAME]` table, Hekma uses its built-in table for well-known providers: OpenAI, Anthropic, Google's Gemini API, Mistral, Groq, DeepSeek, OpenRouter, xAI, Hugging Face's router and Azure OpenAI. Each comes with its inference paths and its own key header. An agent whose upstream matches neither is refused at start, and the error says to declare the origin beside the key.

The origin is fixed on the Run when custody starts. A relaunch after the agent's configuration changes still sends the key to the same origin. A call to a path outside the list is refused with `400 hekma_path_not_allowed` and nothing is forwarded, so a Run Key cannot start batch or file jobs, or reach key-management endpoints.

Give Hekma a key scoped to inference on the projects and models your agents use. A key that can manage other keys is more than any agent needs.

### Which calls get the key

Only a call that presents a Run Key issued to a supervised agent, to a Run opened through the Control API with credentials, or to a Run announced on `POST /v0/runs/announce` (which presented an announce credential), can use the custodied key — in the API-key slot (`authorization`, `x-api-key`, `x-goog-api-key` or `api-key`) or in `x-hekma-run-key`. A child Run of a custodied agent uses its parent's key. An endpoint binding's custodied key is used only by such a credential, never by a call that arrives with none.

Any other call is forwarded with whatever key it carries, or none. A Run Key issued through the `x-hekma-announce` header never draws custody: whoever sent the header got it, with no credential. A Run Key in the API-key slot whose Run holds no key, or that is not eligible, is refused with `400 hekma_custody_unavailable`.

Under the swap, the proxy removes every credential the client sent and inserts the custodied key once, in the provider's own header. If the provider echoes the key in a response, the proxy replaces it with `hekma_redacted_key` before the agent sees it. It asks the provider for an uncompressed response so the check can see the bytes.

When you change a key in the secrets file, the proxy picks up the new value within 30 seconds. The value lives only in the memory of `hekma serve`.

### Seeing it

`hekma ps` (in the CONTROL column) and `hekma run show` (a CUSTODY line) show custody for each Run that owns a process: `on`, or `off` with the reason:

| Reason | Meaning |
|---|---|
| `no-held-key` | The agent has no `provider.api_key`. |
| `opted-out` | `provider.custody` is `off`. |
| `pre-custody` | The Run started before this version; its next launch picks custody up. |

`hekma agent config get my-agent provider.api_key --reveal` still prints the key, but for a custodied key it first records a `secret_revealed` event naming the secret (never the value). If the event cannot be recorded, nothing is printed.

An agent whose provider key Hekma does not hold sends its own key. A self-reported one shows `supervised (no wire)`. An engine-observed one is refused at start: set `provider.api_key`, or switch the adapter to self-reported.

### Threat model

Key custody keeps your provider key away from the agent. It is not a sandbox.

#### What custody protects

- **Your key never reaches the agent.** The agent's environment, command line and Agent Home hold a Run Key and the proxy's address, never your provider key. So anything the agent prints, writes, uploads or commits — its logs, a transcript sent to its vendor, a crash report, a `.env` file pushed by mistake, an answer to a prompt that asks for its API key — can contain at most its Run Key.
- **Your key goes only where you pinned it.** The proxy sends it only over HTTPS, with certificate checks, to the provider origins and paths listed beside the key in your secrets file (or the built-in default for well-known providers). Changing an agent's configuration cannot send it anywhere else.
- **Your key is spent only with a credential Hekma issued.** A call reaches your key only with a Run Key Hekma issued. Any other call through the proxy is forwarded with whatever key it carries, or none.
- **A Run Key is worth less than your key.** It works only through the proxy, only for its Run, under that Run's budgets and Policies, and you can revoke it at any time; it is refused from its next call. It changes every time a supervised agent is relaunched, and it stops working when the Run ends.
- **Going around the proxy fails.** The Run Key is not a provider key, so an agent that calls the provider directly with it is refused by the provider.

#### What custody does not protect against

- **Anything running as your OS user.** The agent runs as you. A process running as you — the agent's own shell tool included — can read Hekma's secrets file, which sits in Hekma's state directory beside the agents' homes; run `hekma` with your authority, including `hekma agent config get --reveal`; and edit Hekma's configuration. A `--reveal` of a custodied key is recorded as an event, but a determined process does not need it. Running agents as a separate OS user is not supported yet.
- **A compromised agent.** Custody limits what an agent can leak by accident or be talked into leaking. It does not contain an agent that sets out to misuse your machine.
- **Other credentials.** An agent that finds another way to reach a provider — a login stored in your home directory, another environment, a different machine — is not using your custodied key. Hekma does not meter those calls; [bypass detection](#bypass-detection-preview) can notice a direct connection from a supervised agent's process tree.
- **Hekma's own memory.** While `hekma serve` runs, it holds your key in memory. On Linux it marks itself non-dumpable, so other processes running as you cannot read its environment or memory and a crash writes no core file. On macOS and Windows, a process running as you, or a full crash dump, may expose it; leave full crash dumps off for `hekma`.
- **Copies of the secrets file.** Backups and sync tools copy Hekma's state directory like any other. Exclude the secrets file, or keep your keys in the environment of `hekma serve` instead.

#### Limits to know

- **Agents that log in instead of using an API key** (a subscription or OAuth sign-in) cannot use custody. Hekma meters them from their own reports where it can, and says so in `hekma ps`.
- **Your home directory stays visible by default.** Many agents need your Git and SSH settings, so the agent keeps your home directory. Turning on home isolation (`provider.isolate_home`) for an agent keeps it from finding your other stored logins by default, at the cost of those settings; it does not stop a shell command that reads them by full path.
- **Custody inference keys, not admin keys.** Give Hekma a key scoped to the projects and models your agents use. A key that can manage other keys is more than any agent needs.
- **On a shared machine,** another user can listen on the proxy's port while Hekma is not running and collect Run Keys and prompts. Run Hekma on a machine you do not share.
- **Run Keys reach your agent's logs.** If an agent prints its environment, its Run Key lands in its output log. Revoke it if that log leaves your machine.

## Scent-Check Modes

A **Scent-Check Mode**, set per [Policy](policies.md), decides what the metering proxy does with a call that carries no credential Hekma issued.

A call is **scented** when a valid Run Key identifies it: sent in `x-hekma-run-key`, or in the API-key slot under [key custody](#key-custody-preview--not-yet-supported). Every other call is **unscented**, including:

- a call on a labelled endpoint binding, or a split Run beneath it;
- a call that lands on the unattributed Run;
- a call on an agent's own Run route (`/_hekma/runs/<run id>`) that does not present its Run Key. A supervised agent's base URL names its Run, but that is not an identity: any local process can use the same URL. The agent becomes scented once custody is on, or once it sends the Run Key it already has in `HEKMA_RUN_KEY`.

| Mode | An unscented call is |
|---|---|
| `off` | Served, unchecked. |
| `shadow` | Served. The Run it lands on gets the `unscented` **Anomaly** on the first such call (one event; `hekma ps` lists the Run), and the proxy counts it (the `hekma.calls.unscented` OTLP counter, `outcome=served`). |
| `enforce` | Refused with `401 hekma_unscented_call`, before anything is forwarded. The per-call record says `enforce`, and `hekma.calls.unscented` counts it under `outcome=refused`. |

- **The default is `off`, everywhere.** The default Policy is seeded `off`, and Hekma never changes it on upgrade, so a default Policy you edited stays as you left it. There is no per-environment default: to check identities, write a Policy, for example `hekma policy create prod --selector env=prod --scent-check shadow`.
- **The strictest mode up the chain applies.** A call is checked under the strictest mode of its Run and every Run above it, so a child Run under an `enforce` parent is `enforce`, whatever Policies its own labels select.
- **Only tighter.** A Run's own Caller (its Run Key) may raise its Run's mode with `scent_check` on `PATCH /v0/runs/{id}/limits`, never lower it. The Operator sets modes on Policies.
- **An unknown or revoked Run Key is refused under every mode** (`401 hekma_unknown_credential`, `401 hekma_credential_revoked`): a key that fails is a failed authentication, not a missing identity.
- **Breach Actions do not change.** A latched `block` still refuses first with its `402`, under every mode, and `warn`, `pause` and `stop` behave the same under every mode. Unlike `block`, `enforce` has no exemption for `GET`, `HEAD` or `OPTIONS`.
- **Announcing is served under every mode.** A call that [announces](announce.md) is served under `enforce` too: announcing is how a client obtains the identity the mode asks for. Under `shadow` the announced Run gets the `unscented` Anomaly; its later calls that present its Run Key are scented.
- **Attribution, not access control.** On your machine any process can obtain an identity, by announcing one; Scent-Check is about knowing which Run a call belongs to. Custody's protection does not depend on the mode.
- `enforce` is never a default.

## Bypass detection (preview)

When an agent runs in its own [Shim](hosting.md#agents-in-their-own-shim-a-preview), the Shim checks the agent's process tree every 5 seconds for established TCP connections to a known provider host, and reports what it finds on the agent's Run.

**The provider-host list.** The built-in provider hosts (the ones key custody knows), every endpoint binding's upstream, every open Run's upstream, and every origin pinned beside a held key. Every agent is checked against the whole list, so an agent calling a provider that is not its own is noticed too. Hekma resolves the hosts to addresses, re-resolves them every 60 seconds and keeps the last good addresses when a lookup fails; the Shim compares each connection's address and port.

| The Run | A connection to a listed host is |
|---|---|
| Key custody on | The `bypass` **Anomaly**: `hekma ps` lists the Run with `bypass[<host>]`, and the OTLP counter `hekma.bypass.detected` counts it by provider host. |
| Key custody off | Informational: `hekma ps --all` notes the host and that custody is off, so direct calls are expected. |
| No wire path (the agent holds its own provider key) | Informational: noted the same way, with that reason. |

- Each host is reported once per Run, at most 8 hosts, each as a `bypass-detected` Run event. Hosts stay on the Run until it ends.
- Detection never stops, pauses or fences a Run.
- **The detection state.** `hekma run show` and `--json` carry every supervised Run's state: `active` (checked every 5 seconds; no hosts means none was seen), `unavailable` with the reason (the process tree could not be read), or `unreported` (no report yet: the first check is pending, or the agent does not run in a Shim). Hekma never reports a Run as checked without having checked it.
- **Where it works.** Linux (from `/proc`) and macOS (from the kernel's per-process socket information). Windows does not run agents in a Shim yet, so its Runs stay `unreported`; other Unix systems report `unavailable`.

**Limits.** Bypass detection is a signal, not a guarantee:

- It notices a connection. It does not prove a call was made, and it does not contain an agent.
- Traffic through another proxy, VPN or tunnel shows as a connection to that proxy, not to the provider.
- HTTP/3 runs over UDP, not TCP, so it is not seen.
- A connection that opens and closes between two checks can be missed. One held across two checks is always caught.
- Only the agent's process tree is watched. A process outside it, or one that left it, is not.
- Hosts that share addresses (behind one CDN) are named together, and a host whose addresses changed since the last lookup can be missed for up to a minute. A finding names hosts; it is not a verdict.
- A Shim keeps its findings while no engine is connected and hands them to the next one. A Shim that exits first loses them.

## What is coming

### Held calls

When wire hold ships, calls held at the proxy will be re-checked when they are released, so a key revoked while a call waits is refused then.

### An authenticated proxy beyond loopback

On main. A metering proxy bound beyond loopback (`--metering-listen 0.0.0.0:<port>`) requires TLS (`--proxy-tls-cert`, `--proxy-tls-key`) and does not start without it. Every call from another machine carries a credential: a Run Key of a Run Hekma did not start, or the endpoint binding's **proxy-access credential** (`hk_pxy_`) in the API-key slot. That credential is shown once and stored as a hash, works only on its binding's prefix, is swapped for the binding's custodied key and never forwarded, and is not a Run's identity for Scent-Check. A supervised agent's Run Key is refused from another machine. Key custody through a proxy beyond loopback is a preview like the rest of custody: it is called supported only once its security criteria pass in CI. See [the metering proxy](metering-proxy.md#beyond-loopback), including [port squatting](metering-proxy.md#port-squatting).

## What it does not promise

- A process running as your user can read the agent's environment, and could already act as the Operator through `control.json`. `state.db` holds only hashes, so a copy of it reveals no key.
- An agent that prints its own environment to its output log puts its key there. Revoke it with `hekma run revoke-keys`.
- Possession is the only binding: whoever holds a Run Key or a refresh credential can use it.
- Key custody keeps the provider key away from the agent's process and files once it ships. It does not contain a compromised agent. Hekma is not a sandbox, and key custody will get a security review before these docs describe it as supported.

## Read next

- [Runs](runs.md): what a Run is and the Control Depth ladder.
- [Control API](control-api.md#run-keys): the credential classes, the routes and the error codes.
- [Commands](commands.md#run-keys): `hekma run keys` and `hekma run revoke-keys`.
- [Policies](policies.md): Selectors, limits and the default Policy.
- [Meter token usage](https://hekma.ktesio.dev/docs/metering/): the metering that works today.
