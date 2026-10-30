# Security review: ARCHITECTURE-SPINE.md, control-plane amendment (2026-10-30)

- **Lens:** security. In scope: AD-20 (credential resolver, TLS beyond loopback), AD-22 (tokens, hashing, refresh binding, key custody, header stripping), AD-23 (Shim socket and named-pipe access control), AD-26 (proxy identity order, endpoint bindings, socket handoff), AD-27 (token hashes and per-call records at rest).
- **Inputs read:** the spine (working tree, lines cited below), today's memlog rulings, PRD §3, FR-41, FR-43, FR-51..FR-60, NFR-4/6/9, §6, §8, proposal §4.2, and the code under `crates/`.
- **Ground rules applied:** `ktesio-*` names are not findings. The stated limits are accepted: no sandbox claim (NFR-6), refresh bound by possession only (AD-22), and same-user processes being able to read the tokens an agent was given. Deferred items are accepted when they cannot let two units diverge.
- **Verdict:** **not ready for stories as written.** Two defects ship insecure systems if built exactly as written. AD-22/AD-26 insert the custodied key with no identity check, which makes the loopback proxy a confused deputy for any local user or a DNS-rebinding web page. And custody does not hold against a same-user agent that looks for the key. Four more are high: the Shim channel and the socket-handoff channel have no access control, the credential classes have no authority matrix, and the unauthenticated identity header can name any Run. The rest are medium or low tightening items. Each one has replacement text below.

## Not findings (checked and accepted)

- **Unsalted SHA-256 for `hk_*` tokens.** The tokens are 256-bit uniformly random, so they need no pepper, no salt and no slow hash. A state.db leak does not let anyone brute-force them. This holds only for the four minted classes; see F12 for the Operator bearer.
- **Timing of the hash-index lookup.** The lookup is keyed on SHA-256(token). What a HashMap probe reveals is the hash, not the token, and preimage resistance protects it. Hashing attacker-chosen bytes first also removes HashDoS. A constant-time compare is not needed on this path.
- **Possession-based refresh.** It is stated in AD-22 and FR-53, and OS attestation is in Deferred. Accepted. F9 only asks for rotation and revocation inside that model.
- **The existing Control API bearer compare.** `crates/hekma-host/src/control/auth.rs:67-76` is constant-time over content and refuses ambiguous multi-header presentations. It is sound.
- **Per-call record fields as listed in AD-26.** {call id, run id, Control Depth, model, status class, latency, tokens} carry no content and no secret. F10 covers the one field the PRD adds ("Run Key identity").

---

## F1: CRITICAL. The custodied key is inserted on every call, so the loopback proxy is a confused deputy

**Evidence.**
- AD-22 (spine l.280): "The proxy strips the agent's `Authorization` / `x-api-key` and inserts the real key."
- AD-26 (l.306) gives the per-call sequence: "strip identity and credential headers, apply the snapshot's disposition … swap in the custodied key, forward". Neither text ties insertion to the call's identity. Nothing maps a custodied key to the virtual provider keys or instances allowed to spend it.
- The proxy is one shared listener on a fixed port. It needs no credential on loopback (l.306), and the default Scent-Check Mode is `off`/`shadow` (PRD FR-53 gate, §13 Q9). A call with no identity, or with a Run Key the caller minted for itself through the announce header (FR-52), therefore reaches the "swap in" step.
- Today's listener drops the inbound `Host` header without validating it and has no `Origin` check (`crates/hekma-engine/src/metering/listener.rs:561-568`).

**Failure scenario.** Key custody is on for one supervised instance. Any of these can then spend the custodied provider key and read the responses:
- another OS user on the same machine,
- a container that shares the network namespace,
- a web page in the Operator's browser, through DNS rebinding to `127.0.0.1:<fixed port>`.

Every one of these calls succeeds, built exactly as the spine says.

**Fix.** Replace the last sentence of the AD-22 custody bullet with:

> A custodied key is mapped by the Operator to the virtual provider keys and supervised instances allowed to spend it. The proxy inserts it **only** when the call's identity resolves to one of those virtual provider keys, or to the Run Key of a supervised Run whose instance holds that mapping. A self-announced Run Key, an identity header, an endpoint binding or no identity never receives a custodied key. Such a call keeps its own credential headers, or is refused under `enforce`. Only when it inserts a custodied key does the proxy strip the caller's credential headers.

In AD-26 change "strip identity and credential headers" to "strip identity headers (and credential headers only when a custodied key is inserted, AD-22)", and add:

> The proxy refuses a request whose `Host` is not a loopback literal or an Operator-configured name, and a request carrying an `Origin` header not on an Operator allow-list (DNS rebinding, browser CSRF).

## F2: HIGH. Key custody does not keep the key from a same-user agent, yet Deferred calls privilege separation "superseded"

**Evidence.**
- AD-22 (l.280) resolves custodied keys "through `SecretResolver`". Its v1 resolvers are the process environment and the engine secrets file, mode 0600 and owned by the Operator's user (AD-10; `crates/hekma-engine/src/ports/secret_resolver.rs:267`).
- Supervised agents run as that same OS user. So the agent can `cat` the secrets file at its well-known state-dir path. When the key came from the environment, it can also read `/proc/<hekma serve pid>/environ`, or `/proc/<shim pid>/environ` because the Shim is its parent (AD-23).
- AD-22 makes the child environment allowlist mandatory but says nothing about the Shim's own environment.
- Deferred (l.542) reads: "~~Privilege separation and attested egress~~ — superseded 2026-10-30 by key custody (AD-22) plus bypass detection".
- The FR-53 threat model promises that custody "keeps the provider key away from the agent process and its files". An LLM agent with a shell tool that is told to "find an API key" does exactly these reads, and that is the bypass the pivot set out to fix.

**Failure scenario.** The custody story ships, and the docs promise that the real key never reaches the agent. A coding agent runs `grep -r sk- ~/.local/share/hekma` or reads its parent's environ and gets the key. FR-54 may flag a later direct call, best-effort, but by then the key has been disclosed.

**Fix.**
1. Add to the AD-22 custody bullet:

   > Custodied keys resolve only from the secrets file (or the Deferred keychain resolver), never from the env resolver. The Service Host and every Shim launch with provider variables and every resolved `secret:` value removed from their own environment. The Shim spawns the child with a cleared environment plus the allowlist.

2. Add to the threat-model sentence:

   > Custody holds against an agent that uses only what it is given. Any process running as the Operator's OS user can still read the secrets file and the Service Host's memory. Making custody a boundary requires running agents as a separate OS user.

3. Change the Deferred line to:

   > Privilege separation and attested egress: *narrowed*, not superseded, by key custody plus bypass detection. Running agents as a separate OS user stays Deferred as the only boundary against a same-user agent that goes looking.

## F3: HIGH. AD-23's Shim socket and named pipe have no access control

**Evidence.**
- AD-23 (l.286): "a Unix domain socket / named-pipe client: spawn asks the Shim, stop/pause/poll are RPCs, adopt reconnects to the socket". The spine gives no location, no file mode, no peer check and no pipe DACL.
- Through the spawn RPC the Shim receives the full child spec, including resolved `secret:` env values and the Run Key and refresh credential (AD-22: "to the process the Engine (via its Shim) launched").
- The Shim also owns stdin (AD-12, AD-19), so a connecting peer can write agent input. For an agent with tools, that is code execution as the agent.
- The state dir is created with umask permissions and never set to 0700: `crates/hekma-engine/src/domain/registry.rs:1653-1658` (`ensure_dir` → `create_dir_all`). There is no `set_permissions` on directories anywhere in the engine or the host.
- The Windows default named-pipe DACL grants read access to Everyone and Anonymous.
- macOS limits `sun_path` to 104 bytes, and `~/Library/Application Support/…` paths overflow it. That pushes an implementer toward `/tmp`.

**Failure scenario.** Another local user, or a process that squats on a freed path or pipe name, does one of these:
- connects to a Shim and injects stdin, stops or pauses the agent, or reads the spawn spec's secrets;
- plants a fake Shim at a recorded endpoint, which makes the Service Host "adopt" a dead process (a false handoff success, contrary to FR-55);
- on Windows, pre-creates the pipe name.

**Fix.** Add a bullet to AD-23:

> **Channel access control:**
> - The Shim endpoint lives in a per-user directory the engine creates mode 0700 and verifies as owner-only before use: the state dir, or `$XDG_RUNTIME_DIR/hekma` when `sun_path` would overflow. Never a shared `/tmp`. The socket is mode 0600.
> - Both ends check the peer: same uid via `SO_PEERCRED` / `getpeereid` / `LOCAL_PEERCRED`. On adopt, the engine also checks that the peer pid and start time match the Shim fingerprint in the spawn record.
> - On Windows the pipe is created with an owner-only DACL, `PIPE_REJECT_REMOTE_CLIENTS` and `FILE_FLAG_FIRST_PIPE_INSTANCE`. The client checks `GetNamedPipeServerProcessId` against the recorded Shim.
> - The Shim's own argv carries no secret. Spawn specs travel only over the checked channel.
> - The engine state dir, state.db and the calls store are created owner-only (0700 for directories, 0600 for files).

## F4: HIGH. The listening-socket handoff channel is unauthenticated, so the proxy socket can be hijacked

**Evidence.** AD-26 (l.307): "the outgoing process passes the listening socket to its successor (Unix `SCM_RIGHTS` over the handoff channel or systemd socket activation; Windows `WSADuplicateSocket`)". The spine never defines the handoff channel: where it is, who may connect, or how the outgoing process knows the peer is its legitimate successor. `WSADuplicateSocket` duplicates into whatever pid the peer names.

**Failure scenario.** During a graceful restart, a local process connects to the handoff endpoint first and receives the proxy's listening socket. From then on it receives every agent's model traffic: virtual provider keys, Run Keys, refresh credentials and prompts. It can answer with forged completions or replay the tokens to the real proxy later. The outgoing process reports a clean handoff.

**Fix.** Add to the AD-26 Restart bullet:

> The handoff channel is a Unix domain socket or named pipe with the same access control as the Shim channel (AD-23 channel bullet: owner-only directory, peer uid check, owner-only DACL, `FILE_FLAG_FIRST_PIPE_INSTANCE`). The outgoing process hands the socket only to a peer that has the same uid **and** has proved it is the next state-dir owner, by presenting a one-time nonce the outgoing process wrote mode 0600 into the state dir for this restart. The listening socket, the state-dir ownership lock and the nonce move together. A handoff that fails these checks closes the channel, keeps serving, and reports the restart as failed. Exec-based upgrade, where the successor inherits the descriptor, and systemd socket activation need no channel and are preferred where available.

## F5: HIGH. The credential resolver has classes but no authority matrix, so classes can escalate

**Evidence.**
- AD-20 (l.195-197): "one credential resolver with classes — operator/Host bearer, announce-scoped credential, Run Key, refresh credential, Node credential (AD-24) — each looked up by hash". The spine never says which class may call which route or verb, or that a Run-scoped token may act only on its own Run.
- AD-26 (l.306) asks for "TLS plus a credential required beyond loopback (AD-20)", and FR-51 lists "bearer" among the accepted credentials. Read literally, the Operator's Control API bearer becomes something handed to agent clients.
- The announce, heartbeat and close endpoints all sit "under the announce-scoped credential" (l.206-207), although FR-52 says that credential may never "act on another Run".
- Inbound OTLP (FR-51, the spine diagram `OBS -- OTLP --> API`) has no stated credential. The Control API's single bearer layer (`crates/hekma-host/src/control/mod.rs:87-133`) means a story either hands agents the Operator bearer or exempts the route.

**Failure scenarios.**
- A Run Key or announce-scoped credential accepted at `/v0/runs/{other id}` lets a Caller act on another Run.
- One leaked announce-scoped credential lets anyone close or heartbeat every Run. That hides `silent` Runs and ends their bookkeeping.
- An agent configured with the Operator bearer as its proxy credential can raise its own ceilings and change Policies.
- An unauthenticated OTLP route lets anyone renew any Run's Lease.

**Fix.** Add to AD-20 under *Binding and auth*:

> **Authority matrix (each token's class comes from its stored row, never from the presented prefix):**
>
> | Class | Where it is accepted | What it may do |
> |---|---|---|
> | Operator/Host bearer | Control API only | Every route. Never accepted by the Metering Proxy and never a Caller credential. |
> | Announce-scoped credential | The announce endpoint and inbound OTLP | Create Runs and set Caller labels. Nothing else. |
> | Run Key | The Metering Proxy, and Control API routes scoped to its own run id (and that Run's children) | Caller verbs only: tighten limits, heartbeat, close. |
> | Refresh credential | The refresh endpoint | Refresh its own Run's key. Nothing else. |
> | Node credential | Reserved | Decided in AD-24. |
>
> The proxy beyond loopback accepts a Run Key, a virtual provider key or a dedicated proxy-access class. It never accepts the Operator/Host bearer. A token's run scope is checked against the path's run id in the core, not in handlers.

## F6: HIGH. The identity header is unauthenticated yet can name any Run, and endpoint labels can steer a keyed Run's Policy

**Evidence.**
- AD-26 (l.306) sets the identity order: "Run Key, virtual provider key, identity header, endpoint binding". FR-51 defines the identity header as "run id and labels, never content" and FR-43 attributes child Runs by it. It carries no secret.
- The spine never limits which Runs a header may name, and never sets the label source for header labels. AD-25 only has `source ∈ bound|caller`.
- Nothing says whether endpoint-path labels attach to a call that is already identified by a Run Key. FR-60's guard ("a Caller cannot choose its way into a looser Policy") breaks if they do, because the caller chooses the path.

**Failure scenarios.**
- Under `off` or `shadow` (the defaults), a local process sends calls naming a supervised Run's run id. The forged usage trips that Run's `stop` or `block`, which is an unrequested stop of a victim (against the spirit of NFR-9). Or the process charges its own spend to another Run and escapes its budget.
- A Run Key holder calls through `/team-lax/…` to pick up the labels, and so the Policy, of a looser endpoint.

**Fix.** Add to AD-26:

> An identity header may name only a Run whose own identity is weaker than a key: a base-URL Run formed by an endpoint binding, or a child Run whose parent is the call's authenticated identity. A header naming a Run that holds a Run Key, a supervised Run, or a Run under another parent is ignored. The call is recorded as unattributed with an Anomaly. Header labels are `caller` labels. Endpoint-binding labels are bound only on the Run that the binding itself identifies. A call that carries a Run Key or virtual provider key never gains labels from the path it arrives on.

## F7: MEDIUM. Custodied keys are not bound to an upstream origin (reroute, plaintext upstream)

**Evidence.**
- AD-26 (l.306) orders the steps as "apply the snapshot's disposition — … reroute (a per-call upstream from configured routes) … — swap in the custodied key, forward". The key is chosen per Run or instance, not per destination, and nothing names a route's credential.
- The upstream validator accepts `http://` for any host: `crates/hekma-engine/src/metering/listener.rs:431-449`.

**Failure scenario.**
- A reroute to a route on another provider or another gateway sends provider A's real key to host B.
- A custodied instance whose `upstream_base_url` is `http://gateway.internal` sends the real key in cleartext on the network.

**Fix.** Add to the AD-22 custody bullet:

> A custodied key is bound to one upstream origin (scheme, host and port). The proxy inserts it only when the outbound origin, after any reroute, equals that origin. A configured route names its own `secret:` credential or forwards none. Custody refuses an `http://` upstream whose host is not loopback.

## F8: MEDIUM. Minted tokens can be delivered on argv or into files, and the File/Flag refusal covers the wrong value

**Evidence.**
- AD-22 (l.280): "refuses `File` and `Flag` config targets for custodied keys". Custodied keys are, by the same sentence, "never passed to the adapter config mapping", so this refusal guards a value that never enters the mapping.
- The values that do enter the mapping are the virtual provider key, through the reserved override, and the Run Key and refresh credential for supervised Runs. They are covered by nothing.
- The adapter contract supports all three target kinds: `crates/hekma-adapter-api/src/config.rs:54-75` (`Env`, `Flag`, `File`).

**Failure scenario.** A manifest maps the reserved virtual-key override to `--api-key`. The token then appears in `ps` output, which every local user can read on Linux and macOS. Or a `File` target writes the token into the Agent Home.

**Fix.** Replace the clause with:

> refuses `File` and `Flag` targets for every value the engine mints or holds for an agent (virtual provider key, Run Key, refresh credential) and for any `secret:` value under custody: these are delivered only through `Env` targets, and, like the `metering.base_url` override, are never written to `effective-config.json`.

## F9: MEDIUM. Refresh credentials never rotate or expire on relaunch, Run Key expiry is not enforced at lookup, and a held call is not re-checked on release

**Evidence.**
- AD-22 (l.279): "a Restart Policy relaunch gets a fresh one". It does not say the previous refresh credential or Run Key is revoked, that a refresh rotates the refresh credential, or that expiry is checked at lookup. "Revocation deletes the row and republishes; the next call fails" covers explicit revocation only.
- FR-56 release forwards held calls "unchanged", with no re-check of identity after a revocation or expiry.

**Failure scenario.**
- A refresh credential copied once, by a subprocess or a log, mints Run Keys for the whole life of the Run, even across relaunches. Nothing detects that two holders exist.
- An expired Run Key keeps working until a sweeper deletes its row.
- A call held before revocation is released afterwards with the custodied key.

**Fix.** Add to AD-22:

> Every token row stores an expiry, and the proxy and resolver check it at lookup from the snapshot. A refresh rotates both the Run Key and the refresh credential. Presenting a superseded refresh credential revokes the Run's tokens and raises an Anomaly (reuse detection, still possession-based). A relaunch revokes the previous process's tokens before issuing new ones. Releasing a held call re-resolves its identity against the current snapshot, and a call whose identity was revoked or expired ends with the typed rejection.

## F10: MEDIUM. Token minting and plaintext handling are unspecified: CSPRNG, type, records, transport

**Evidence.**
- AD-22 (l.279) says "opaque 256-bit random tokens" but names no random source. AD-25 names `getrandom` for run ids only.
- The engine has no direct CSPRNG dependency: `getrandom` arrives only through `ring`, which `rustls` pulls in (`cargo tree -i getrandom@0.2.17`). The house precedent under NFR-8 is to hand-roll rather than add a crate (`crates/hekma-engine/src/sha256.rs`). The existing id minting is a counter plus wall time (`crates/hekma-engine/src/domain/usage.rs:61-72`).
- AD-22 says lookups are "never with `SecretString::eq`" but never says what type holds the plaintext between minting and its one response.
- FR-43 requires "Run Key identity" in per-call records. The spine does not define it, so a story may store the token there.
- The spine never says that tokens travel only in headers.

**Failure scenario.**
- A story mints tokens from `RandomState` hashing or a time-seeded PRNG to avoid a new dependency, so the tokens are predictable.
- The announce response DTO derives `Debug` and gets logged with the plaintext Run Key and refresh credential in it.
- Per-call records or outbox events (which are exported to Hosts) carry the token.
- An SSE client passes `?token=` in a URL, and the URL lands in proxy and access logs.

**Fix.** Add to AD-22:

> Tokens are drawn from the OS CSPRNG (`getrandom` as a direct dependency, or `ring::rand::SystemRandom`, already in-tree). They never come from `RandomState`, time, counters or a userspace PRNG. A CSPRNG failure refuses to mint. From minting to its single response write, the plaintext lives only in a redacting type (`SecretString` or equivalent) whose response DTO `Debug` redacts it. Events, the outbox, per-call records and logs carry a non-secret **key id** (the row id), never the token or its lookup hash. Tokens are accepted only in headers, never in a URL query.

## F11: MEDIUM. "Strip identity and credential headers" names no set, so tokens can leak upstream

**Evidence.**
- AD-22 names `Authorization` / `x-api-key` only (l.280), and AD-26 says "identity and credential headers" (l.306).
- Providers that OpenAI-compatible clients reach also take credentials in `api-key` (Azure), `x-goog-api-key`, and `?key=` (Gemini).
- FR-51 adds outbound identity headers "so a gateway in front of the provider can attribute them". The spine does not say these never carry a token.

**Failure scenario.** A client sends its virtual provider key in `api-key` or `?key=`. Under custody the proxy inserts the real key in `Authorization` and also forwards `hk_vk_…` to the provider or gateway, where it is logged. Or an outbound identity header built from the inbound identity copies the Run Key.

**Fix.** Add to AD-26:

> The Metering Proxy's inbound identity and credential set is fixed and versioned: `Authorization`, `x-api-key`, `api-key`, `x-goog-api-key`, the `key` query parameter, and every `x-hekma-*` header. Under custody, or whenever a value in that set carries an `hk_` prefix, that value is removed before forwarding. Outbound identity headers carry the run id and bound labels only, never a token or key id.

## F12: MEDIUM. The Operator bearer has no strength floor beyond loopback and could be stored as an unsalted hash

**Evidence.**
- AD-20 (l.196-197) has every class "looked up by hash (AD-22)", and AD-22 (l.279) says "stored only as a SHA-256 hash in state.db".
- The Operator bearer, unlike the `hk_*` classes, is operator-chosen through `--bearer-secret <name>` (`crates/hekma-host/src/main.rs:94-111`). No length or entropy check exists, and the Control API has no failed-auth throttle.
- AD-20 says nothing about the custody of the TLS private key.

**Failure scenario.** A story unifies storage and writes SHA-256 of a short Operator bearer into state.db. A state.db copy then cracks it offline in seconds. Or a weak bearer exposed beyond loopback (FR-57's remote `hekma top`) is brute-forced online.

**Fix.** Add to AD-20:

> The Operator/Host bearer stays resolved through `SecretResolver` and is compared in memory. It is never written to state.db. A non-loopback bind refuses to start with a bearer under 32 bytes (`hekma token new` mints an `hk_op_` token). Failed authentication beyond loopback is rate-limited per peer. The TLS private key resolves like a secret (`secret:` name or an owner-only file checked like the secrets file).

## F13: LOW. The fixed loopback proxy port can be squatted on a shared host, and the threat model does not say so

**Evidence.**
- AD-26 (l.306-307): one listener "on a fixed port, loopback by default … else the CLI's engine for its lifetime", and "A crash refuses connections until the next process binds".
- Wire Runs (FR-51) keep `base_url = http://127.0.0.1:<port>` in their own config. While no engine-owning process is running, which in CLI-only use is most of the time, any local user can bind that port.

**Failure scenario.** On a VM shared by several OS users, another user binds the port during a crash or between CLI runs. They receive the wire Runs' virtual keys, Run Keys and prompts, and replay the keys to the real proxy later.

**Fix.** Add to the AD-22 docs sentence:

> The loopback proxy authenticates callers, not itself. On a host shared by several OS users, any of them can bind the proxy port while no engine-owning process holds it. The docs say so, and recommend systemd socket activation (the port is held across restarts and crashes) or a single-user host.

## F14: LOW. "`sha2`, in-tree" misstates where SHA-256 lives

**Evidence.**
- AD-22 (l.279) says "(`sha2`, in-tree)". `sha2 v0.11.1` is a dependency of the `hekma` CLI crate only (`cargo tree -i sha2`).
- The engine, where the resolver index and the store live, has its own std-only, FIPS-vector-pinned `crates/hekma-engine/src/sha256.rs`.

**Failure scenario.** A story adds `sha2` to `hekma-engine` on the strength of "in-tree" (an NFR-8 delta and a second SHA-256 implementation), or hesitates over which one is authoritative.

**Fix.** Replace it with "(the engine's std-only `sha256.rs`, story 17-4)".
