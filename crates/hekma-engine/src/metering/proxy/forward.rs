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
             {} bad target, {} busy",
            c.credential_in_url.load(Ordering::Relaxed),
            c.unknown_credential.load(Ordering::Relaxed),
            c.credential_revoked.load(Ordering::Relaxed),
            c.custody_unavailable.load(Ordering::Relaxed),
            c.out_of_scope.load(Ordering::Relaxed),
            c.no_route.load(Ordering::Relaxed),
            c.bad_target.load(Ordering::Relaxed),
            c.busy.load(Ordering::Relaxed),
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
            control_depth: self.depth,
            binding: self.binding.clone(),
            disposition,
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
            pending.pump.close_call(record, usage, permit);
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
                api_key: header(headers, "x-api-key"),
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

    // The outbound headers: hop-by-hop, Host and every `x-hekma-*` stripped;
    // credential values marked sensitive; identity headers added.
    strip_hop_by_hop(&mut parts.headers);
    wire::strip_hekma_headers(&mut parts.headers);
    parts.headers.remove(hyper::header::HOST);
    for name in [
        hyper::header::AUTHORIZATION,
        hyper::header::PROXY_AUTHORIZATION,
        HeaderName::from_static("x-api-key"),
    ] {
        if let Some(value) = parts.headers.get_mut(&name) {
            value.set_sensitive(true);
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
    let latched = shared.snapshot.current().disposition_over(&run_id);
    if let Some(value) = wire::breach_header_value(&Disposition::reasons(latched.breach_latched)) {
        res_parts
            .headers
            .insert(HeaderName::from_static(wire::BREACH_HEADER), value);
    }
    let mut response = Response::new(Full::new(res_bytes));
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
