---
title: "Hekma in front of a gateway"
description: "Hekma is not a model gateway. Put it in front of LiteLLM, Portkey or a similar gateway: it meters and governs each call, and adds the Run's id and labels, which the gateway can use to attribute it."
---

# Hekma in front of a gateway

> **On main, not yet released.** The switch below ships with the next release. The two recipes are **examples**, not validated integrations; see [What these recipes are](#what-these-recipes-are).

> This page mirrors [hekma.ktesio.dev/docs/gateways/](https://hekma.ktesio.dev/docs/gateways/), where it is kept current.

**Hekma is not a model gateway.** It meters every model call it forwards, attributes it to a [Run](runs.md), checks it against that Run's budgets, and lets an Operator hold, reroute or refuse a Run's calls. Multi-provider routing, response caching, fallback chains and an organisation-wide key vault stay with a gateway such as LiteLLM or Portkey. The two work together: the client calls Hekma's [metering proxy](metering-proxy.md), and the proxy forwards to the gateway.

## What the gateway sees

By default every call the proxy forwards carries two **identity headers**:

- `x-hekma-run-id`: the Run's id;
- `x-hekma-labels`: all of the Run's labels, bound and Caller, as sorted, percent-encoded `key=value` items joined by commas (`job=7,team=core`).

The headers are available to the gateway; whether it can use them to attribute a call is up to the gateway (to verify against the gateway's docs).

The identity headers never carry request or response content. Hekma never sends the gateway:

- any Hekma credential: a Run Key, a refresh credential, an announce or proxy-access credential, or a token id;
- a limit, a total, a cost, or an agent instance's name;
- a provider key Hekma holds, unless the gateway's origin is pinned for that key (see [Key custody through a gateway](#key-custody-through-a-gateway)).

Every `x-hekma-*` header a client sends is removed before the call is forwarded, whether the identity headers are on or off.

## Turning the identity headers off

The headers send each call's Run id and labels to whatever the call already goes to. Turn them off when that destination should not see them. There are three levels, and the most specific one wins:

1. **A binding's own setting.** `hekma binding set <name> --identity-headers on|off|inherit`. `inherit` follows the proxy setting. A supervised agent's calls have no binding and follow the proxy setting.
2. **A serve flag, for one run.** `hekma serve --no-identity-headers` or `--identity-headers` overrides the stored setting until that host stops. It is never stored, and the host's started notice says when it disagrees with the stored setting.
3. **The proxy setting.** `hekma proxy set identity-headers off` (or `PATCH /v0/proxy`) is stored in the state directory, so an auto-started host reads it too. It applies to the very next forwarded call, with no restart. `hekma proxy show` prints it.

All three are Operator surfaces. An agent cannot change them: none of them is an agent configuration key.

## Recipe A: a binding in front of a gateway

For a client that can only change its base URL.

1. Create a binding whose upstream is the gateway's base URL (the URL form and port are to verify against the gateway's docs):

   ```sh
   hekma binding create litellm --prefix /litellm \
     --upstream http://127.0.0.1:4000/v1 --label team=core
   ```

2. Point the client's base URL at the binding's client base URL, which `hekma binding show litellm` prints (`http://127.0.0.1:<port>/litellm`).
3. The client keeps sending the gateway's own key in its usual header (the header name is to verify against the gateway's docs). The proxy forwards it unchanged.
4. To send the identity headers to this gateway while the proxy setting is off, run `hekma binding set litellm --identity-headers on`.

The first call creates the binding's `wire` Run, which carries `team=core`.

## Recipe B: a supervised agent in front of a gateway

For an agent Hekma starts, metered on the wire (`engine-observed`).

1. Set the instance's upstream to the gateway's base URL (to verify against the gateway's docs):

   ```sh
   hekma agent config set my-agent metering.upstream_base_url http://127.0.0.1:4000/v1
   ```

2. Configure the gateway's key as the instance's provider secret, as for any engine-observed agent. For a gateway on plain `http`, also set `provider.custody = "off"`: key custody only sends a key over `https`.
3. Start the agent. Its calls reach the gateway through the proxy, on its Run route, and carry the proxy setting's identity headers.

## Key custody through a gateway

[Key custody](run-keys.md#key-custody-preview--not-yet-supported) is a preview. A held key is only ever sent over `https` to an origin and paths pinned for it. A gateway is not in the built-in provider table, so declare its origin, and the paths it serves, beside the key in the secrets file. A reroute route carries its own key or none.

## What these recipes are

Examples. Hekma's half of each recipe is tested against an OpenAI-compatible fake upstream. Neither gateway is run in Hekma's CI, which has no network, so every gateway-side fact here is marked "to verify against the gateway's docs". A recipe will be called validated only after a recorded pass against the real gateway.

## Read next

- [The metering proxy](metering-proxy.md): the full wire contract.
- [Attach](attach.md): every way Hekma reaches a client.
- [Control API](control-api.md#endpoint-bindings-and-the-metering-proxy): `GET` and `PATCH /v0/proxy`.
