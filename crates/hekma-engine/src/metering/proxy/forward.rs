//! The call state machine of the shared Metering Proxy (story 23-1).
//!
//! Per call, before any upstream dial and reading only the published
//! snapshot: refuse a bad target, take a concurrency slot, identify the Run
//! ([`super::identify`]), ensure a new wire Run through the writer (the only
//! hot-path wait on it, bounded), take a record permit, apply the `block`
//! disposition over the Run's ancestor chain, strip every `x-hekma-*`
//! header, add the outbound identity headers, check the outbound URI's
//! authority, forward (never following a redirect), relay the buffered
//! response with the breach header, and submit the call's records.
//!
//! A client that disconnects drops this future: the upstream request is
//! cancelled with it and a drop guard records `client-disconnect` (or
//! `shutdown` while the host stops) with unknown tokens.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, Uri};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::identify::{identify, Attribution, CallFacts, Identified, Refusal, Via};
use super::records::{new_call_id, RecordPump};
use super::wire::{self, WireError};
use crate::domain::calls::{
    record_model, CallDisposition, CallOutcome, CallRecord, CallStart, StatusClass,
};
use crate::domain::pipeline::ProxiedUsage;
use crate::domain::runs::snapshot::{Disposition, SnapshotPublisher};
use crate::domain::runs::wire::{ensure_wire_run, Ensured, WireIdentity};
use crate::domain::RunId;
use crate::metering::parse::{parse_openai_sse_usage, parse_openai_usage};
use crate::metering::relay::{
    drain_request_body, inject_include_usage, is_sse, strip_hop_by_hop, ForwardClient,
    MAX_RELAY_BODY_BYTES,
};
use crate::ports::ProcessFingerprint;

/// Counters of calls refused before a Run was identified (noted at most once
/// a minute, never recorded against a Run).
#[derive(Debug, Default)]
pub(crate) struct RefusalCounters {
    pub credential_in_url: AtomicU64,
    pub unknown_credential: AtomicU64,
    pub credential_revoked: AtomicU64,
    pub custody_unavailable: AtomicU64,
    pub out_of_scope: AtomicU64,
    pub no_route: AtomicU64,
    pub bad_target: AtomicU64,
    pub bad_host: AtomicU64,
    pub path_not_allowed: AtomicU64,
    /// Story 22-3: unscented calls served under `shadow` and refused under
    /// `enforce`.
    pub unscented_served: AtomicU64,
    pub unscented_refused: AtomicU64,
    pub busy: AtomicU64,
    pub ignored_hints: AtomicU64,
    pub capped_hints: AtomicU64,
    /// Responses buffered past the byte budget: the provider had already
    /// billed the call, so it is recorded and relayed, never refused.
    pub relay_overshoot: AtomicU64,
    /// The minute of the last capped-hint note.
    pub hint_note_minute: AtomicU64,
    /// The minute of the last over-budget relay note.
    pub overshoot_note_minute: AtomicU64,
}

/// True at most once a minute per `last` (a rate limit for notes that
/// must never be one per call).
fn once_a_minute(last: &AtomicU64) -> bool {
    let minute = crate::time::now_unix_secs() / 60;
    let seen = last.load(Ordering::Relaxed);
    seen != minute
        && last
            .compare_exchange(seen, minute, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
}

/// One pending Run creation per identity: concurrent first calls share it.
pub(crate) type PendingCreations =
    std::sync::Mutex<std::collections::HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>>;

/// Everything a call reads, shared by every connection.
pub(crate) struct ProxyShared {
    pub snapshot: Arc<SnapshotPublisher>,
    pub pump: Arc<RecordPump>,
    pub client: ForwardClient,
    pub identity_headers: bool,
    pub inflight: Arc<Semaphore>,
    /// The admission slots' total.
    pub max_calls: usize,
    /// The buffered-byte budget, in KiB permits.
    pub buffered_kib: Arc<Semaphore>,
    pub counters: RefusalCounters,
    /// This proxy's own `127.0.0.1:<port>` (an absolute-form target naming
    /// it is accepted).
    pub authority: String,
    pub owner: Option<ProcessFingerprint>,
    pub stopping: Arc<AtomicBool>,
    /// One pending creation per identity (concurrent first calls share it).
    pub pending: PendingCreations,
    pub diagnostics: Arc<dyn Fn(String) + Send + Sync>,
    /// The minute of the last pre-identification refusal note.
    pub last_note_minute: AtomicU64,
    /// Story 22-2: the custody cache — the swap's value source (in-memory
    /// only; a missing entry means the key was never seeded or its refresh
    /// never succeeded, and the call is refused, never forwarded bare).
    pub custody: Arc<crate::domain::custody::CustodyCache>,
    /// Story 22-2: the Host header values beyond the loopback literals the
    /// guard accepts (`--proxy-allowed-hosts`).
    pub allowed_hosts: Arc<[String]>,
    /// Story 22-2: the `Origin` values the guard accepts
    /// (`--proxy-allowed-origins`); an `Origin` NOT on the list is refused.
    pub allowed_origins: Arc<[String]>,
    /// cfg(test) seam: slow each Run creation down inside the writer, so a
    /// creation queued behind it is not taken within the ack bound.
    #[cfg(test)]
    pub ensure_delay: std::sync::Mutex<Option<std::time::Duration>>,
}

impl ProxyShared {
    fn count(&self, refusal: Refusal) {
        let counter = match refusal {
            Refusal::CredentialInUrl => &self.counters.credential_in_url,
            Refusal::CustodyUnavailable => &self.counters.custody_unavailable,
            Refusal::UnknownCredential => &self.counters.unknown_credential,
            Refusal::CredentialRevoked => &self.counters.credential_revoked,
            Refusal::RunOutOfScope => &self.counters.out_of_scope,
            Refusal::NoRoute => &self.counters.no_route,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.note_refusals();
    }

    /// At most one note a minute summarising the pre-identification
    /// refusals (never one per call: refused calls must not flood logs).
    fn note_refusals(&self) {
        if !once_a_minute(&self.last_note_minute) {
            return;
        }
        let c = &self.counters;
        (self.diagnostics)(format!(
            "metering proxy refusals so far: {} credential in URL, {} unknown credential, {} \
             revoked credential, {} Run Key in the API-key slot, {} out of scope, {} no route, \
             {} bad target, {} bad host, {} path outside the held key's list, {} busy, {} \
             unscented served (shadow), {} unscented refused (enforce)",
            c.credential_in_url.load(Ordering::Relaxed),
            c.unknown_credential.load(Ordering::Relaxed),
            c.credential_revoked.load(Ordering::Relaxed),
            c.custody_unavailable.load(Ordering::Relaxed),
            c.out_of_scope.load(Ordering::Relaxed),
            c.no_route.load(Ordering::Relaxed),
            c.bad_target.load(Ordering::Relaxed),
            c.bad_host.load(Ordering::Relaxed),
            c.path_not_allowed.load(Ordering::Relaxed),
            c.busy.load(Ordering::Relaxed),
            c.unscented_served.load(Ordering::Relaxed),
            c.unscented_refused.load(Ordering::Relaxed),
        ));
    }

    /// A hint that could not open its Run (a breadth or nesting cap, or the
    /// parent ended): the call stays on the parent, attributed, and this
    /// says so at most once a minute.
    fn note_capped_hint(&self) {
        let total = self.counters.capped_hints.fetch_add(1, Ordering::Relaxed) + 1;
        if once_a_minute(&self.counters.hint_note_minute) {
            (self.diagnostics)(format!(
                "metering proxy: a split or child hint could not open its own Run (1,000 Runs \
                 are already open beneath its parent, it is 8 levels deep, or the parent \
                 ended), so the call was counted on the parent Run ({total} such calls so far)"
            ));
        }
    }

    /// A response buffered past the byte budget: recorded and relayed,
    /// because the provider already billed it; said at most once a minute.
    fn note_relay_overshoot(&self) {
        let total = self
            .counters
            .relay_overshoot
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if once_a_minute(&self.counters.overshoot_note_minute) {
            (self.diagnostics)(format!(
                "metering proxy: a response was buffered past the buffered-byte budget and \
                 relayed anyway, because the provider had already billed the call ({total} \
                 such responses so far)"
            ));
        }
    }
}

fn refusal_error(refusal: Refusal) -> WireError {
    match refusal {
        Refusal::CredentialInUrl => WireError::CredentialInUrl,
        Refusal::CustodyUnavailable => WireError::CustodyUnavailable,
        Refusal::UnknownCredential => WireError::UnknownCredential,
        Refusal::CredentialRevoked => WireError::CredentialRevoked,
        Refusal::RunOutOfScope => WireError::RunOutOfScope,
        Refusal::NoRoute => WireError::NoRoute,
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A `Host` header value without its port (`[::1]:80` → `::1`,
/// `name:8478` → `name`, `name` → `name`).
fn host_name_part(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map_or(rest, |(inside, _)| inside);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

/// Whether a `Host` header value names a loopback literal (`127.0.0.0/8`,
/// `::1`, `localhost` — the port stripped, brackets kept honest) — the DNS
/// rebinding half of the boundary: only the proxy's own loopback addresses
/// are ever acceptable by default.
fn host_is_loopback_literal(host: &str) -> bool {
    let hostname = host_name_part(host);
    if hostname.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = hostname.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// The Host/Origin guard (story 22-2): `Host` must be a loopback literal or
/// on the Operator's `--proxy-allowed-hosts` list; `Origin`, when present,
/// must be on `--proxy-allowed-origins` (browser pages send `Origin` on
/// cross-site requests; SDKs send none). The refusal precedes
/// identification and any dial, and no `Access-Control-*` header is ever
/// added — a page's read of a response body fails in the browser regardless.
fn host_origin_ok(
    headers: &HeaderMap,
    allowed_hosts: &[String],
    allowed_origins: &[String],
) -> bool {
    let listed = |value: &str, list: &[String]| {
        list.iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(value.trim()))
    };
    let host_ok = match header(headers, hyper::header::HOST.as_str()) {
        Some(host) => {
            host_is_loopback_literal(host)
                || listed(host, allowed_hosts)
                || listed(host_name_part(host), allowed_hosts)
        }
        None => false,
    };
    if !host_ok {
        return false;
    }
    match header(headers, "origin") {
        Some(origin) => listed(origin, allowed_origins),
        None => true,
    }
}

/// Replace every exact occurrence of `needle` in `haystack`, or `None`
/// when there is none (byte-wise: a body need not be UTF-8).
fn replace_bytes(haystack: &[u8], needle: &[u8], mask: &[u8]) -> Option<Vec<u8>> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut index = 0;
    let mut found = false;
    while index < haystack.len() {
        if haystack[index..].starts_with(needle) {
            out.extend_from_slice(mask);
            index += needle.len();
            found = true;
        } else {
            out.push(haystack[index]);
            index += 1;
        }
    }
    found.then_some(out)
}

/// Response redaction (story 22-2): every exact occurrence of the custodied
/// key in the relayed headers and the buffered body becomes the fixed mask.
/// A header whose value cannot hold the masked text is dropped rather than
/// relayed with the key; a changed body gets its `content-length` rewritten.
fn redact_response(headers: &mut HeaderMap, body: Bytes, needle: &[u8]) -> Bytes {
    let mask = wire::REDACTION_MASK.as_bytes();
    let mut drop: Vec<HeaderName> = Vec::new();
    for (name, value) in headers.iter_mut() {
        if let Some(replaced) = replace_bytes(value.as_bytes(), needle, mask) {
            match HeaderValue::from_bytes(&replaced) {
                Ok(mut masked) => {
                    masked.set_sensitive(value.is_sensitive());
                    *value = masked;
                }
                Err(_) => drop.push(name.clone()),
            }
        }
    }
    for name in drop {
        headers.remove(name);
    }
    match replace_bytes(&body, needle, mask) {
        Some(masked) => {
            headers.insert(
                hyper::header::CONTENT_LENGTH,
                HeaderValue::from(masked.len()),
            );
            Bytes::from(masked)
        }
        None => body,
    }
}

/// The canonical (scheme, host, port) of a URI: lowercased host, no
/// trailing dot, the effective port.
fn canonical(uri: &Uri) -> Option<(String, String, u16)> {
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let host = uri.host()?.trim_end_matches('.').to_ascii_lowercase();
    let port = uri.port_u16().or(match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    })?;
    Some((scheme, host, port))
}

/// The outbound URI: `<upstream base> + <rest> + <query>`, refused unless
/// its scheme, host and port equal the upstream's (the `@`-in-path class of
/// tricks cannot move a call to another authority).
pub(crate) fn outbound_uri(upstream: &str, rest: &str, query: Option<&str>) -> Option<Uri> {
    let base = upstream.trim().trim_end_matches('/');
    let built = match query {
        Some(query) => format!("{base}{rest}?{query}"),
        None => format!("{base}{rest}"),
    };
    let uri: Uri = built.parse().ok()?;
    let expected: Uri = base.parse().ok()?;
    (canonical(&uri)? == canonical(&expected)?).then_some(uri)
}

/// Whether the request target is one the proxy forwards: origin-form, or
/// absolute-form naming this proxy itself. `CONNECT` and `*` never are.
fn target_ok(method: &Method, uri: &Uri, authority: &str) -> bool {
    if *method == Method::CONNECT {
        return false;
    }
    if uri.path_and_query().is_some_and(|pq| pq.as_str() == "*") {
        return false;
    }
    match uri.authority() {
        None => true,
        Some(given) => given.as_str().eq_ignore_ascii_case(authority),
    }
}

/// The record pieces a call carries from admission to close.
struct Pending {
    pump: Arc<RecordPump>,
    permit: Option<OwnedSemaphorePermit>,
    call_id: String,
    run_id: RunId,
    token_id: Option<String>,
    depth: &'static str,
    binding: Option<String>,
    started_at: String,
    started: Instant,
    upstream_started: Option<Instant>,
    stopping: Arc<AtomicBool>,
    /// Story 22-3: served unscented under `shadow` (the close raises the
    /// Run's `unscented` Anomaly).
    unscented: bool,
}

impl Pending {
    fn record(
        &self,
        disposition: CallDisposition,
        outcome: CallOutcome,
        status: Option<u16>,
        status_class: StatusClass,
        model: Option<String>,
        usage: Option<ProxiedUsage>,
    ) -> CallRecord {
        CallRecord {
            call_id: self.call_id.clone(),
            run_id: self.run_id.clone(),
            token_id: self.token_id.clone(),
            control_depth: self.depth.to_string(),
            binding: self.binding.clone(),
            disposition,
            hold_end: None,
            outcome,
            status,
            status_class,
            model,
            input_tokens: usage.map(|u| u.input),
            output_tokens: usage.map(|u| u.output),
            cached_tokens: usage.map(|u| u.cached),
            started_at: self.started_at.clone(),
            ended_at: crate::time::now_rfc3339_micros(),
            latency_us: self.started.elapsed().as_micros() as u64,
            upstream_us: self
                .upstream_started
                .map(|at| at.elapsed().as_micros() as u64),
        }
    }
}

/// Closes a call's record exactly once: explicitly, or on drop (the client
/// went away, or the host aborted the call while stopping).
struct CloseGuard(Option<Pending>);

impl CloseGuard {
    fn get(&mut self) -> &mut Pending {
        self.0.as_mut().expect("a call closes once")
    }

    fn close(
        &mut self,
        disposition: CallDisposition,
        outcome: CallOutcome,
        status: Option<u16>,
        status_class: StatusClass,
        model: Option<String>,
        usage: Option<ProxiedUsage>,
    ) {
        if let Some(mut pending) = self.0.take() {
            let record = pending.record(disposition, outcome, status, status_class, model, usage);
            let permit = pending.permit.take();
            pending
                .pump
                .close_call(record, usage, pending.unscented, permit);
        }
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        if let Some(pending) = &self.0 {
            let outcome = if pending.stopping.load(Ordering::Acquire) {
                CallOutcome::Shutdown
            } else {
                CallOutcome::ClientDisconnect
            };
            self.close(
                CallDisposition::Pass,
                outcome,
                None,
                StatusClass::TransportError,
                None,
                None,
            );
        }
    }
}

/// Ensure a wire Run through the writer, sharing one pending creation per
/// identity. `Err(())` → the writer did not take it within the ack bound.
async fn ensure(shared: &ProxyShared, identity: WireIdentity) -> Result<Ensured, ()> {
    let key = identity.key();
    let gate = PendingGate::join(&shared.pending, key.clone());
    let _held = gate.gate.lock().await;
    // A concurrent first call may have created it while this one waited.
    if let Some(run) = shared.snapshot.current().identities.get(&key) {
        return Ok(Ensured::Run(run.clone()));
    }
    let Some(handle) = shared
        .pump
        .writer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
    else {
        return Err(());
    };
    let publisher = Arc::clone(&shared.snapshot);
    #[cfg(test)]
    let delay = *shared
        .ensure_delay
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let result = handle
        .call_async(move |ctx| {
            #[cfg(test)]
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            let at = crate::time::now_rfc3339();
            let (ensured, changes) = ensure_wire_run(ctx.store(), &identity, RunId::mint(), &at)?;
            let committed = crate::domain::runs::ops::commit(ctx, &publisher, changes)?;
            Ok((ensured, committed))
        })
        .await;
    match result {
        // The created events are durable in the outbox; they reach the bus
        // through the Supervisor's inbox like the proxied evaluations.
        Ok((ensured, committed)) => {
            shared.pump.inbox.push_run_events(committed);
            Ok(ensured)
        }
        Err(_) => Err(()),
    }
}

/// One call's share of an identity's pending-creation gate. Dropping it, on
/// every return of [`ensure`], removes the map's entry once no other call
/// still holds the gate.
struct PendingGate<'a> {
    pending: &'a PendingCreations,
    key: (String, String),
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> PendingGate<'a> {
    fn join(pending: &'a PendingCreations, key: (String, String)) -> Self {
        let gate = {
            let mut map = pending.lock().unwrap_or_else(|p| p.into_inner());
            Arc::clone(map.entry(key.clone()).or_default())
        };
        Self { pending, key, gate }
    }
}

impl Drop for PendingGate<'_> {
    fn drop(&mut self) {
        let mut map = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        // The map's handle and this one: no other call is waiting.
        if Arc::strong_count(&self.gate) <= 2 {
            map.remove(&self.key);
        }
    }
}

/// Resolve an attribution to an open Run id (creating it when needed).
async fn resolve(shared: &ProxyShared, attribution: Attribution) -> Result<Option<RunId>, ()> {
    match attribution {
        Attribution::Run(run) => Ok(Some(run)),
        Attribution::Ensure {
            identity,
            fallback,
            fallback_identity,
        } => match ensure(shared, identity).await? {
            Ensured::Run(run) => Ok(Some(run)),
            Ensured::Capped | Ensured::ParentGone => {
                shared.note_capped_hint();
                if let Some(run) = fallback {
                    return Ok(Some(run));
                }
                match fallback_identity {
                    Some(identity) => match ensure(shared, identity).await? {
                        Ensured::Run(run) => Ok(Some(run)),
                        _ => Ok(None),
                    },
                    None => Ok(None),
                }
            }
        },
    }
}

/// Serve one request (never panics, never fabricates success).
pub(crate) async fn serve(
    shared: Arc<ProxyShared>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    // Story 22-2: the Host/Origin guard runs FIRST — before the target
    // check, before identification, before any dial — so a rebinding-shaped
    // request never reaches the key. `OPTIONS` preflights meet the same
    // guard (never a permissive answer, never a CORS header).
    if !host_origin_ok(
        req.headers(),
        &shared.allowed_hosts,
        &shared.allowed_origins,
    ) {
        shared.counters.bad_host.fetch_add(1, Ordering::Relaxed);
        drain_request_body(req.into_body()).await;
        shared.note_refusals();
        return WireError::BadHost.response();
    }
    if !target_ok(req.method(), req.uri(), &shared.authority) {
        shared.counters.bad_target.fetch_add(1, Ordering::Relaxed);
        drain_request_body(req.into_body()).await;
        return WireError::BadTarget.response();
    }
    let Ok(slot) = Arc::clone(&shared.inflight).try_acquire_owned() else {
        shared.counters.busy.fetch_add(1, Ordering::Relaxed);
        return WireError::ProxyBusy.response();
    };
    let response = serve_admitted(&shared, req).await;
    drop(slot);
    response
}

async fn serve_admitted(
    shared: &Arc<ProxyShared>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let started = Instant::now();
    let snapshot = shared.snapshot.current();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let identified = {
        let headers = req.headers();
        identify(
            &snapshot,
            &CallFacts {
                path: &path,
                query: query.as_deref(),
                run_key: header(headers, wire::RUN_KEY_HEADER),
                authorization: header(headers, hyper::header::AUTHORIZATION.as_str()),
                // Story 22-2: every provider's API-key header is a slot
                // (Anthropic's, Google's, Azure's), so a Run Key placed in
                // any of them is read — never forwarded as if it were a key.
                api_key: header(headers, "x-api-key")
                    .or_else(|| header(headers, "x-goog-api-key"))
                    .or_else(|| header(headers, "api-key")),
                hint: header(headers, wire::RUN_HINT_HEADER),
                hint_labels: header(headers, wire::LABELS_HEADER),
            },
        )
    };
    let (target, attribution, hint_ignored) = match identified {
        Identified::Refused(refusal) => {
            shared.count(refusal);
            drain_request_body(req.into_body()).await;
            return refusal_error(refusal).response();
        }
        Identified::Call {
            target,
            attribution,
            hint_ignored,
        } => (target, attribution, hint_ignored),
    };
    if hint_ignored {
        shared
            .counters
            .ignored_hints
            .fetch_add(1, Ordering::Relaxed);
    }
    let run_id = match resolve(shared, attribution).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            shared.count(Refusal::NoRoute);
            drain_request_body(req.into_body()).await;
            return WireError::NoRoute.response();
        }
        Err(()) => {
            drain_request_body(req.into_body()).await;
            return WireError::RunUnavailable.response();
        }
    };
    // Re-read after a creation (its publish preceded the ack).
    let snapshot = shared.snapshot.current();
    // A pending Run route (the launch job has not committed yet) has no
    // entry: the Run is supervised and carries no label yet.
    let entry = snapshot.run(&run_id).cloned();
    if entry.is_none() && !snapshot.pending_routes.contains_key(&run_id) {
        drain_request_body(req.into_body()).await;
        return WireError::NoRoute.response();
    }
    let depth = entry
        .as_ref()
        .map(|entry| entry.record.control_depth.as_str())
        .unwrap_or("supervised");
    let labels: Vec<(String, String)> = entry
        .as_ref()
        .map(|entry| {
            entry
                .labels
                .iter()
                .map(|label| (label.key.clone(), label.value.clone()))
                .collect()
        })
        .unwrap_or_default();

    // The permit: none within 1 s → refused before any dial.
    let Some(permit) = shared.pump.permit().await else {
        drain_request_body(req.into_body()).await;
        return WireError::LedgerUnavailable.response();
    };
    let binding = match &target.via {
        Via::Binding(name) => Some(name.clone()),
        Via::RunRoute | Via::LegacyRoot => None,
    };
    let mut guard = CloseGuard(Some(Pending {
        pump: Arc::clone(&shared.pump),
        permit: Some(permit),
        call_id: new_call_id(),
        run_id: run_id.clone(),
        token_id: target.token_id.clone(),
        depth,
        binding: binding.clone(),
        started_at: crate::time::now_rfc3339_micros(),
        started,
        upstream_started: None,
        stopping: Arc::clone(&shared.stopping),
        unscented: false,
    }));

    // The disposition over the whole ancestor chain.
    let over: Disposition = snapshot.disposition_over(&run_id);
    if over.breach_block != 0 && !wire::is_block_exempt(req.method()) {
        let reasons = Disposition::reasons(over.breach_block);
        if let Some(refusal) = crate::domain::fence_refusal(&reasons) {
            let awaits_continue = req
                .headers()
                .get(hyper::header::EXPECT)
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"100-continue"));
            if !awaits_continue {
                drain_request_body(req.into_body()).await;
            }
            guard.close(
                CallDisposition::Block,
                CallOutcome::Refused,
                Some(refusal.status),
                StatusClass::Refused,
                None,
                None,
            );
            return wire::block_response(&refusal);
        }
    }

    // Story 22-3 — the Scent-Check Mode, after the fence (a fenced Run's
    // 402 wins) and before the call-start record and any dial. A call is
    // SCENTED when a valid Run Key identified it; everything else (a
    // binding's Run, a split, the unattributed Run, a Run route reached
    // with no key) is unscented. The effective mode is the strictest over
    // the attribution Run's ancestor chain. No safe-method exemption:
    // identity does not depend on the method.
    if target.token_id.is_none() {
        match snapshot.scent_over(&run_id) {
            crate::domain::runs::policy::ScentCheckMode::Off => {}
            crate::domain::runs::policy::ScentCheckMode::Shadow => {
                guard.get().unscented = true;
                shared
                    .counters
                    .unscented_served
                    .fetch_add(1, Ordering::Relaxed);
            }
            crate::domain::runs::policy::ScentCheckMode::Enforce => {
                let awaits_continue = req
                    .headers()
                    .get(hyper::header::EXPECT)
                    .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"100-continue"));
                if !awaits_continue {
                    drain_request_body(req.into_body()).await;
                }
                guard.close(
                    CallDisposition::Enforce,
                    CallOutcome::Refused,
                    Some(401),
                    StatusClass::Refused,
                    None,
                    None,
                );
                shared
                    .counters
                    .unscented_refused
                    .fetch_add(1, Ordering::Relaxed);
                shared.note_refusals();
                return WireError::UnscentedCall.response();
            }
        }
    }

    // The call-start record.
    {
        let pending = guard.get();
        pending.pump.start(CallStart {
            call_id: pending.call_id.clone(),
            run_id: pending.run_id.clone(),
            token_id: pending.token_id.clone(),
            control_depth: pending.depth,
            binding: pending.binding.clone(),
            started_at: pending.started_at.clone(),
            owner: shared.owner,
        });
    }

    let Some(uri) = outbound_uri(&target.upstream, &target.rest, query.as_deref()) else {
        drain_request_body(req.into_body()).await;
        guard.close(
            CallDisposition::Pass,
            CallOutcome::Refused,
            None,
            StatusClass::Refused,
            None,
            None,
        );
        shared.counters.bad_target.fetch_add(1, Ordering::Relaxed);
        return WireError::BadTarget.response();
    };

    // Story 22-2 — the custody swap's gates, before the body is buffered
    // and before any dial: the outbound scheme is `https` with certificate
    // verification on (the client is built with the webpki store and no
    // bypass exists), the outbound ORIGIN equals the origin frozen on the
    // Run (canonical equality: scheme, lowercased host, effective port),
    // and the outbound PATH is on the key's allow-list. A failure refuses
    // with nothing forwarded and the upstream seeing nothing.
    let mut custody = None;
    if let Some(info) = target.custody.clone() {
        let name = crate::domain::config::secret_name(&info.key_ref)
            .unwrap_or_default()
            .to_string();
        let entry = match shared.custody.get(&name) {
            Some(entry) => Some(entry),
            // A miss (a binding's key, a custodied Run adopted after a
            // restart): resolve once, off the async threads.
            None => {
                let cache = Arc::clone(&shared.custody);
                let lookup = name.clone();
                tokio::task::spawn_blocking(move || cache.resolve_now(&lookup))
                    .await
                    .ok()
                    .flatten()
            }
        };
        let Some(entry) = entry else {
            drain_request_body(req.into_body()).await;
            guard.close(
                CallDisposition::Pass,
                CallOutcome::Refused,
                None,
                StatusClass::Refused,
                None,
                None,
            );
            shared.count(Refusal::CustodyUnavailable);
            return WireError::CustodyUnavailable.response();
        };
        let scheme_https = uri.scheme_str() == Some("https");
        let origin_pinned =
            scheme_https && crate::domain::custody::origins_equal(&uri.to_string(), &info.origin);
        if !origin_pinned {
            drain_request_body(req.into_body()).await;
            guard.close(
                CallDisposition::Pass,
                CallOutcome::Refused,
                None,
                StatusClass::Refused,
                None,
                None,
            );
            shared.counters.bad_target.fetch_add(1, Ordering::Relaxed);
            return WireError::BadTarget.response();
        }
        let paths = crate::domain::custody::allowed_paths(entry.declared.as_ref(), &info.origin);
        if !crate::domain::custody::path_allowed(&paths, uri.path()) {
            drain_request_body(req.into_body()).await;
            guard.close(
                CallDisposition::Pass,
                CallOutcome::Refused,
                None,
                StatusClass::Refused,
                None,
                None,
            );
            shared
                .counters
                .path_not_allowed
                .fetch_add(1, Ordering::Relaxed);
            shared.note_refusals();
            return WireError::PathNotAllowed.response();
        }
        custody = Some((info, entry));
    }

    // The request body (capped), and the byte budget.
    let declared = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > MAX_RELAY_BODY_BYTES as u64) {
        guard.close(
            CallDisposition::Pass,
            CallOutcome::Refused,
            Some(413),
            StatusClass::Refused,
            None,
            None,
        );
        return WireError::BodyTooLarge.response();
    }
    let (mut parts, body) = req.into_parts();
    // The budget is taken as the bytes arrive, so a body the budget cannot
    // hold is refused before it is buffered, and before any dial.
    let (collected, request_budget) = match collect_budgeted(body, shared, false).await {
        Ok(Budgeted { bytes, permit, .. }) => (bytes, permit),
        Err(CollectError::Busy) => {
            guard.close(
                CallDisposition::Pass,
                CallOutcome::Refused,
                Some(503),
                StatusClass::Refused,
                None,
                None,
            );
            shared.counters.busy.fetch_add(1, Ordering::Relaxed);
            return WireError::ProxyBusy.response();
        }
        Err(CollectError::TooLarge) => {
            guard.close(
                CallDisposition::Pass,
                CallOutcome::Refused,
                None,
                StatusClass::Refused,
                None,
                None,
            );
            return WireError::BodyTooLarge.response();
        }
        Err(CollectError::Body) => {
            guard.close(
                CallDisposition::Pass,
                CallOutcome::ClientDisconnect,
                None,
                StatusClass::TransportError,
                None,
                None,
            );
            return WireError::UpstreamError.response();
        }
    };
    let request_model = serde_json::from_slice::<serde_json::Value>(&collected)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string));
    let mut req_bytes = collected;
    inject_include_usage(
        &parts.method,
        &target.rest_or_path(&path),
        &parts.headers,
        &mut req_bytes,
    );

    // The outbound headers: hop-by-hop, Host and every `x-hekma-*` header
    // stripped; credential values marked sensitive; identity headers added.
    strip_hop_by_hop(&mut parts.headers);
    wire::strip_hekma_headers(&mut parts.headers);
    parts.headers.remove(hyper::header::HOST);
    for name in [
        hyper::header::AUTHORIZATION,
        hyper::header::PROXY_AUTHORIZATION,
        HeaderName::from_static("x-api-key"),
        HeaderName::from_static("x-goog-api-key"),
        HeaderName::from_static("api-key"),
    ] {
        if let Some(value) = parts.headers.get_mut(&name) {
            value.set_sensitive(true);
        }
    }
    // Story 22-2 — the swap: under custody the presented credential
    // headers LEAVE (the Run Key itself, or a client's own key — a
    // custodied Run spends only its custodied key, which is what makes
    // attribution total) and the custodied key is inserted ONCE, in the
    // provider's scheme for the frozen origin, its `HeaderValue` marked
    // sensitive so no `Debug` path ever prints it.
    if let Some((info, entry)) = &custody {
        for name in [
            hyper::header::AUTHORIZATION,
            hyper::header::PROXY_AUTHORIZATION,
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("x-goog-api-key"),
            HeaderName::from_static("api-key"),
        ] {
            parts.headers.remove(&name);
        }
        // The response must be readable to be redacted: ask for it
        // uncompressed (a key echoed inside a gzip body would pass the
        // byte scan unseen).
        parts.headers.insert(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        let scheme = crate::domain::custody::scheme_for(&info.origin);
        match scheme.header_value(&entry.value) {
            Some((name, value)) => {
                parts.headers.insert(name, value);
            }
            None => {
                guard.close(
                    CallDisposition::Pass,
                    CallOutcome::Refused,
                    None,
                    StatusClass::Refused,
                    None,
                    None,
                );
                shared.counters.bad_target.fetch_add(1, Ordering::Relaxed);
                return WireError::BadTarget.response();
            }
        }
    }
    if shared.identity_headers {
        if let Ok(value) = HeaderValue::from_str(run_id.as_str()) {
            parts
                .headers
                .insert(HeaderName::from_static(wire::RUN_ID_HEADER), value);
        }
        if let Some(value) = wire::labels_header_value(&labels) {
            parts
                .headers
                .insert(HeaderName::from_static(wire::LABELS_HEADER), value);
        }
    }
    let mut builder = Request::builder().method(parts.method.clone()).uri(uri);
    if let Some(headers) = builder.headers_mut() {
        *headers = parts.headers;
    }
    let Ok(outbound) = builder.body(Full::new(req_bytes)) else {
        guard.close(
            CallDisposition::Pass,
            CallOutcome::UpstreamError,
            None,
            StatusClass::TransportError,
            None,
            None,
        );
        return WireError::UpstreamError.response();
    };

    guard.get().upstream_started = Some(Instant::now());
    let upstream = match shared.client.request(outbound).await {
        Ok(response) => response,
        Err(_) => {
            guard.close(
                CallDisposition::Pass,
                CallOutcome::UpstreamError,
                None,
                StatusClass::TransportError,
                None,
                None,
            );
            return WireError::UpstreamError.response();
        }
    };
    drop(request_budget);
    let (mut res_parts, res_body) = upstream.into_parts();
    let status = res_parts.status.as_u16();
    // The provider has billed the call by now: a response the budget cannot
    // hold is still buffered (up to the body cap), recorded and relayed, and
    // the overshoot is said, never a refusal that drops the usage.
    let (res_bytes, _response_budget) = match collect_budgeted(res_body, shared, true).await {
        Ok(Budgeted {
            bytes,
            permit,
            overshot,
        }) => {
            if overshot {
                shared.note_relay_overshoot();
            }
            (bytes, permit)
        }
        Err(_) => {
            guard.close(
                CallDisposition::Pass,
                CallOutcome::UpstreamError,
                Some(status),
                StatusClass::TransportError,
                None,
                None,
            );
            return WireError::UpstreamError.response();
        }
    };
    let usage = if is_sse(&res_parts.headers) {
        parse_openai_sse_usage(&res_bytes)
    } else {
        parse_openai_usage(&res_bytes)
    }
    .map(|(input, output, cached)| ProxiedUsage {
        input,
        output,
        cached,
    });
    let response_model = serde_json::from_slice::<serde_json::Value>(&res_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string));
    let model = response_model
        .or(request_model)
        .as_deref()
        .and_then(record_model);
    guard.close(
        CallDisposition::Pass,
        CallOutcome::Completed,
        Some(status),
        StatusClass::of_status(status),
        model,
        usage,
    );

    // Relay the buffered response, with the breach header when the Run or
    // an ancestor holds a latched breach of any action.
    strip_hop_by_hop(&mut res_parts.headers);
    // Story 22-2 — response redaction: exact occurrences of the custodied
    // key in the relayed headers and body are replaced with the fixed mask
    // (a provider can echo a presented key in an error body; the response
    // the agent reads must not carry the real key back into everything the
    // agent prints or writes). Responses are buffered whole today; when
    // incremental relay lands the redaction moves to a sliding window.
    let relay_bytes = match &custody {
        Some((_, entry)) => redact_response(
            &mut res_parts.headers,
            res_bytes,
            entry.value.expose_secret().as_bytes(),
        ),
        None => res_bytes,
    };
    let latched = shared.snapshot.current().disposition_over(&run_id);
    if let Some(value) = wire::breach_header_value(&Disposition::reasons(latched.breach_latched)) {
        res_parts
            .headers
            .insert(HeaderName::from_static(wire::BREACH_HEADER), value);
    }
    let mut response = Response::new(Full::new(relay_bytes));
    *response.status_mut() = res_parts.status;
    *response.headers_mut() = res_parts.headers;
    response
}

/// A body collected under the buffered-byte budget.
struct Budgeted {
    bytes: Bytes,
    /// The budget the bytes hold (`None` only for an overshot response
    /// whose first frame already found the budget full).
    permit: Option<OwnedSemaphorePermit>,
    /// Some of the bytes are buffered past the budget (responses only).
    overshot: bool,
}

enum CollectError {
    /// The budget cannot hold the next frame (requests only).
    Busy,
    /// Over the relay body cap.
    TooLarge,
    /// The body failed (the peer went away or broke the framing).
    Body,
}

/// Collect `body` up to [`MAX_RELAY_BODY_BYTES`], taking the buffered-byte
/// budget (KiB permits) as each frame arrives, never after the bytes are
/// already held. A request (`overshoot` false) that finds the budget full
/// fails with [`CollectError::Busy`]; a response (`overshoot` true) keeps
/// buffering past it and reports that it did.
async fn collect_budgeted<B>(
    body: B,
    shared: &ProxyShared,
    overshoot: bool,
) -> Result<Budgeted, CollectError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let mut body = body;
    let mut buf = bytes::BytesMut::new();
    let mut permit: Option<OwnedSemaphorePermit> = None;
    let mut held_kib: usize = 0;
    let mut overshot = false;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| CollectError::Body)?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if buf.len() + data.len() > MAX_RELAY_BODY_BYTES {
            return Err(CollectError::TooLarge);
        }
        buf.extend_from_slice(&data);
        let want_kib = buf.len().div_ceil(1024).max(1);
        if !overshot && want_kib > held_kib {
            match take_budget(shared, want_kib - held_kib) {
                Some(more) => {
                    held_kib = want_kib;
                    match &mut permit {
                        Some(held) => held.merge(more),
                        None => permit = Some(more),
                    }
                }
                None if overshoot => overshot = true,
                None => return Err(CollectError::Busy),
            }
        }
    }
    if permit.is_none() && !overshot {
        // An empty body still holds one KiB, like any other call.
        match take_budget(shared, 1) {
            Some(one) => permit = Some(one),
            None if overshoot => overshot = true,
            None => return Err(CollectError::Busy),
        }
    }
    Ok(Budgeted {
        bytes: buf.freeze(),
        permit,
        overshot,
    })
}

/// Take `kib` KiB permits from the buffered-byte budget.
fn take_budget(shared: &ProxyShared, kib: usize) -> Option<OwnedSemaphorePermit> {
    let kib = u32::try_from(kib).ok()?;
    Arc::clone(&shared.buffered_kib)
        .try_acquire_many_owned(kib)
        .ok()
}

impl super::identify::Target {
    /// The path the `stream_options` injection gates on: the path after the
    /// route prefix (the provider's own path).
    fn rest_or_path(&self, path: &str) -> String {
        if self.rest.is_empty() {
            path.to_string()
        } else {
            self.rest.clone()
        }
    }
}

/// Story 22-2: the pure pieces of the custody swap — response redaction and
/// the Host/Origin guard.
#[cfg(test)]
mod custody_tests {
    use super::*;

    const KEY: &str = "sk-live-0123456789abcdef";

    #[test]
    fn redaction_masks_every_occurrence_byte_wise_and_fixes_the_length() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-echo",
            HeaderValue::from_static("key=sk-live-0123456789abcdef!"),
        );
        headers.insert("content-length", HeaderValue::from_static("999"));
        headers.insert("x-other", HeaderValue::from_static("untouched"));
        // A non-UTF-8 body: the bytes around the key survive exactly.
        let mut body = vec![0xff, 0xfe];
        body.extend_from_slice(KEY.as_bytes());
        body.extend_from_slice(b" and again ");
        body.extend_from_slice(KEY.as_bytes());
        body.push(0x80);
        let out = redact_response(&mut headers, Bytes::from(body), KEY.as_bytes());
        let mut expected = vec![0xff, 0xfe];
        expected.extend_from_slice(wire::REDACTION_MASK.as_bytes());
        expected.extend_from_slice(b" and again ");
        expected.extend_from_slice(wire::REDACTION_MASK.as_bytes());
        expected.push(0x80);
        assert_eq!(out.as_ref(), expected.as_slice());
        assert_eq!(
            headers["content-length"],
            HeaderValue::from(expected.len()),
            "a changed body carries its new length"
        );
        assert_eq!(headers["x-echo"], "key=hekma_redacted_key!");
        assert_eq!(headers["x-other"], "untouched");
    }

    #[test]
    fn a_body_without_the_key_is_relayed_untouched() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("5"));
        let out = redact_response(&mut headers, Bytes::from_static(b"hello"), KEY.as_bytes());
        assert_eq!(out.as_ref(), b"hello");
        assert_eq!(headers["content-length"], "5");
        assert_eq!(replace_bytes(b"", KEY.as_bytes(), b"x"), None);
        assert_eq!(replace_bytes(b"abc", b"", b"x"), None);
    }

    #[test]
    fn a_header_that_cannot_hold_the_mask_is_dropped_never_relayed_with_the_key() {
        let mut headers = HeaderMap::new();
        // An obs-text header value: valid on the wire, and the mask still
        // fits; a value whose replacement would be invalid is dropped.
        let raw = [b"\x80".as_slice(), KEY.as_bytes()].concat();
        headers.insert("x-raw", HeaderValue::from_bytes(&raw).unwrap());
        redact_response(&mut headers, Bytes::new(), KEY.as_bytes());
        let kept = headers.get("x-raw").map(|v| v.as_bytes().to_vec());
        assert!(
            kept.as_deref()
                .is_none_or(|v| !v.windows(KEY.len()).any(|w| w == KEY.as_bytes())),
            "the key never survives in a header"
        );
    }

    #[test]
    fn the_host_name_part_strips_ports_and_brackets() {
        assert_eq!(host_name_part("127.0.0.1:8478"), "127.0.0.1");
        assert_eq!(host_name_part("[::1]:80"), "::1");
        assert_eq!(host_name_part("[::1]"), "::1");
        assert_eq!(host_name_part("localhost"), "localhost");
        assert_eq!(host_name_part("proxy.internal:8478"), "proxy.internal");
        // A bare IPv6 literal (no brackets) is not split on its colons.
        assert_eq!(host_name_part("::1"), "::1");
    }

    fn guard(host: Option<&str>, origin: Option<&str>, hosts: &[&str], origins: &[&str]) -> bool {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(hyper::header::HOST, HeaderValue::from_str(host).unwrap());
        }
        if let Some(origin) = origin {
            headers.insert("origin", HeaderValue::from_str(origin).unwrap());
        }
        let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
        let origins: Vec<String> = origins.iter().map(|o| o.to_string()).collect();
        host_origin_ok(&headers, &hosts, &origins)
    }

    #[test]
    fn the_guard_admits_loopback_literals_and_listed_names_only() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:8478",
            "127.9.9.9:1",
            "[::1]:8478",
            "localhost:1",
        ] {
            assert!(guard(Some(host), None, &[], &[]), "{host}");
        }
        // Rebinding-shaped names, a missing Host, a routable address.
        for host in [
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.nip.io",
            "10.0.0.5:8478",
        ] {
            assert!(!guard(Some(host), None, &[], &[]), "{host}");
        }
        assert!(!guard(None, None, &[], &[]), "no Host");
        // An Operator-listed name, with or without its port.
        assert!(guard(
            Some("proxy.internal"),
            None,
            &["proxy.internal"],
            &[]
        ));
        assert!(guard(
            Some("proxy.internal:8478"),
            None,
            &["proxy.internal"],
            &[]
        ));
        assert!(guard(
            Some("PROXY.internal:8478"),
            None,
            &["proxy.internal"],
            &[]
        ));
    }

    #[test]
    fn any_origin_is_refused_unless_listed() {
        assert!(!guard(
            Some("127.0.0.1"),
            Some("https://evil.example"),
            &[],
            &[]
        ));
        assert!(!guard(Some("127.0.0.1"), Some("null"), &[], &[]));
        assert!(guard(
            Some("127.0.0.1"),
            Some("https://app.example"),
            &[],
            &["https://app.example"]
        ));
    }
}
