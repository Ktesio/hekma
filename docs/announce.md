---
title: "Announce and Leases"
description: "A client Hekma did not start tells Hekma who it is: an announce header on its first proxied call, or a Control API endpoint. Announced Runs hold a Lease; quiet Runs go silent and end."
---

# Announce and Leases

> **On main, not yet released.** Announce and Leases ship with the next release, and so does the OpenTelemetry channel. The CrewAI and LangGraph helpers are still planned; see [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/announce/](https://hekma.ktesio.dev/docs/announce/), where it is kept current.

A framework or plain client that Hekma did not start can still be a [Run](runs.md) of its own. It **announces** an identity, an external id of its choosing such as a job id, and Hekma gives that identity one Run, its own budgets, and a [Run Key](run-keys.md). An announced Run holds a **Lease**, its claim to be alive. A Run whose Lease lapses goes `silent`, and one that stays silent ends.

**Announcing tells Hekma who is calling. It is not authentication: on this machine any process can announce.** Announcing gives a client an identity to budget, list and fence. It does not decide who may reach a model provider. Possession of a Run Key is what binds later calls to the Run.

## Three channels, one identity

| Channel | How | What comes back |
|---|---|---|
| The announce header | `x-hekma-announce: <id>` on a call through the [metering proxy](metering-proxy.md) | On the first claim, the Run Key and refresh credential in the `x-hekma-run-key` and `x-hekma-refresh` response headers |
| The announce endpoint | `POST /v0/runs/announce` on the [Control API](control-api.md#announce-and-heartbeat), under a binding's announce credential | `201` with the Run and its Run Key and refresh credential, or `200` with the renewed Run |
| OpenTelemetry | `POST /v1/metrics` with the resource attribute `hekma.run.external_id`, under a binding's announce credential ([inbound telemetry](telemetry.md#inbound-telemetry-v1metrics)) | Nothing: it creates or renews an `observed` Run and its Lease, and never issues a Run Key |

All three channels claim the same identity, so a client discovered on the wire and announced later stays **one Run**:

- **Beneath a binding,** an announce claims `<id>` in that binding's namespace, the same one an `x-hekma-run` split uses. The Run carries the binding's labels as bound labels, so Policies select it, and the announce's `x-hekma-labels` (or the endpoint's `labels`) as Caller labels.
- **Beneath a Run Key or a Run route,** an announce on the header channel names a **child** Run of that Run. Its usage rolls up to the parent, every ancestor's ceilings apply, and the parent's `block` fences it.

An announce beneath a binding with no labels still claims its own Run. Unlike a hint there, it is never ignored, because it issues a Run Key. An announce and an `x-hekma-run` hint on the same call: the announce wins.

### Shown once, issued once

A Run Key and refresh credential are issued **only when an announce creates the Run**. Announcing an identity whose Run is open again renews its Lease and updates its Caller labels, and issues nothing. A key revoked with `hekma run revoke-keys` therefore stays revoked: announcing again does not bring it back. A client that lost its pair trades its refresh credential for a new one (`POST /v0/runs/refresh`), or closes the Run and announces again.

The credentials are shown once, are not stored, and appear in no read route, event, log or `--json` document. A response that carries them is marked `cache-control: no-store`. Keep them somewhere safe.

### Which key can carry custody

A Run Key from the announce endpoint was issued against a credential, so it can draw on [key custody](run-keys.md#key-custody-preview--not-yet-supported) the way a supervised agent's key does. A Run Key from the announce header was issued to whoever sent the header, so it never draws custody: in `x-hekma-run-key` it names the Run and the call goes out with the client's own provider key, and in the API-key slot it is refused `400 hekma_custody_unavailable`.

### Scent-Check

An announcing call is **served** under every [Scent-Check Mode](run-keys.md#scent-check-modes), `enforce` included: announcing is how a client obtains the identity the mode asks for. Under `shadow` the announced Run carries the `unscented` Anomaly until a Policy change clears it. The Run's later calls that present its Run Key count as scented.

## The announce header, step by step

1. Create a binding for the client (once), and point its base URL at it:

   ```bash
   hekma binding create openai --prefix /openai --upstream https://api.openai.com/v1 --label team=core
   ```

2. On the client's first call, send the header. A job id makes a good identity:

   ```bash
   curl -sS -D - http://127.0.0.1:8400/openai/chat/completions \
     -H "authorization: Bearer $OPENAI_API_KEY" \
     -H "x-hekma-announce: nightly-report-2026-11-13" \
     -H "x-hekma-labels: step=draft" \
     -H "content-type: application/json" \
     -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
   ```

   The call is served as usual. On this first claim its response also carries `x-hekma-run-key: hk_run_…` and `x-hekma-refresh: hk_ref_…`. These headers ride whatever status the call answers, a `402` from a fenced parent included.

3. Send `x-hekma-run-key` with the Run Key on the client's later calls, and drop the announce header. Each call renews the Lease.

`hekma ps` lists the Run once it needs you, and `hekma run show <run id>` shows its LEASE and LAST SEEN lines.

## The announce endpoint

Issue the binding's announce credential once, as the Operator:

```bash
hekma binding credential issue openai --class announce
```

Then the client announces without making a model call. The examples use a Control API on `127.0.0.1:8477` (`hekma serve --listen`); a client on the same machine can use the local endpoint, whose address is in `<state dir>/control.json`.

```bash
curl -sS http://127.0.0.1:8477/v0/runs/announce \
  -H "authorization: Bearer $HEKMA_ANNOUNCE_CREDENTIAL" \
  -H "content-type: application/json" \
  -d '{"external_id": "nightly-report-2026-11-13", "labels": {"step": "draft"}}'
```

A new identity answers `201`: the Run at depth `observed` (it has made no call yet) with `lease_expires_at` set, and `credentials` (`run_key`, `refresh_credential` and their token ids) once. An identity whose Run is open answers `200` with the renewed Run and no credentials. An announce credential announces only in its own binding's namespace, and heartbeats or closes only the Runs it created.

The Run moves to depth `announced` on its first call through the proxy.

## Leases and the windows

An announced Run holds a **Lease**. Any call the proxy attributes to the Run renews it, a refused one included (a client still trying is alive), and so does a heartbeat:

```bash
curl -sS -X POST http://127.0.0.1:8477/v0/runs/<run id>/heartbeat \
  -H "authorization: Bearer $HEKMA_RUN_KEY"
```

A heartbeat answers `204`. The Run's own Run Key (or an ancestor's) and the announce credential that created it may heartbeat it. A heartbeat on a Run that has ended answers `409 run-ended`: announce again to start a new Run.

Runs that hold no Lease (a binding's Run, a split or hint child, the unattributed Run) follow their traffic instead.

| Window | Default | What happens |
|---|---|---|
| Lease | 60 seconds | Extended by every attributed call and every heartbeat |
| Inactivity (Runs with no Lease) | 15 minutes | No traffic for this long: the Run is marked `silent` |
| Silent window | 60 minutes | `silent` for this long: the Run ends with cause `silent-window` |
| Sweep | every 5 seconds | How often the Service Host checks |

**Silence never stops a Run.** A `silent` Run's next call is served as usual, and its budgets, labels and keys are untouched. Its next call or heartbeat brings it back to `active`. A Run that stays silent through the silent window ends: its keys are revoked at once, and its identity's next announce or call creates a new Run. The unattributed Run and a binding's Run end this way too, and come back with the next call.

**A supervised agent is never marked silent.** Its process is its heartbeat: its Run ends when the process does.

The windows are settings of the Service Host, not of a Policy. A Host embedding the engine sets them in `MeteringProxyOptions.leases` (see [Running Hekma as a Host](hosting.md#leases-and-the-sweeper)).

## Closing an announced Run

A Run that is done says so by closing itself, with its own Run Key or the announce credential that created it:

```bash
curl -sS -X POST http://127.0.0.1:8477/v0/runs/<run id>/close \
  -H "authorization: Bearer $HEKMA_RUN_KEY"
```

The Run ends with cause `closed`, its Lease is gone and its keys are revoked.

## Limits

- An external id is 1 to 128 visible ASCII characters. An announce carries at most 16 Caller labels. A Caller label on a key the binding sets is refused (`label-bound`).
- A binding holds at most 1,000 open announced or split Runs. A Run's whole subtree holds at most 1,000 open Runs that Callers opened or announced. Past either cap the endpoint answers `409 too-many-children`. The header channel keeps the call on the parent or binding Run, notes it once a minute, and issues nothing. The Operator and a Host are not capped.
- Anyone on this machine can announce any identity that is not open, and announcing an open identity updates its Caller labels. Treat Caller labels as the client's word, not yours: only bound labels select Policies.

## Read next

- [Runs, Control Depth, ps and top](runs.md)
- [Run Keys and key custody](run-keys.md)
- [The metering proxy](metering-proxy.md)
- [Control API](control-api.md#announce-and-heartbeat)
