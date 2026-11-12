//! Operator endpoint bindings (story 23-1, spine AD-25 / AD-26, D3).
//!
//! A binding says "calls under this path go to this upstream and carry
//! these labels". The shared Metering Proxy matches the longest binding
//! prefix on a segment boundary; a labelled binding's calls go to the
//! binding's own Run (created on the first call), an unlabelled binding's
//! to the single unattributed Run. A binding also stores named reroute
//! routes, an inert custodied-key reference and one announce and one
//! proxy-access credential — nothing accepts either credential in this
//! release.
//!
//! Every change is one writer job and one `binding` event committed with
//! its outbox row; the binding table rides the published snapshot.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::ops::{self, CommittedRunEvent, RunChanges};
use super::snapshot::{BindingEntry, BindingTable, SnapshotPublisher};
use super::RunStatus;
use crate::domain::bus::EngineEvent;
use crate::domain::credential::{self, TokenClass};
use crate::domain::outbox::OUTBOX_MAX_BYTES;
use crate::domain::SecretString;
use crate::ports::{OutboxAppend, RunRecords as _, RunStore as _, StoreError};
use crate::store::proxy_sqlite::{BindingRow, RouteRow};
use crate::store::WriterCtx;

/// The most bindings an engine holds (the hot-path match stays a short scan).
pub const MAX_BINDINGS: usize = 256;

/// The most routes one binding holds.
pub const MAX_BINDING_ROUTES: usize = 16;

/// The longest path prefix.
pub const MAX_PREFIX_CHARS: usize = 128;

/// The reserved prefix (engine-made Run routes live under it).
pub const RESERVED_PREFIX: &str = "/_hekma";

/// [`BindingEvent`]'s schema version.
pub const BINDING_EVENT_SCHEMA_VERSION: u32 = 1;

/// The identity namespace of a binding's own Run (`binding` / `<name>`).
pub(crate) const BINDING_NAMESPACE: &str = "binding";

/// The identity of the single unattributed Run (`proxy` / `unattributed`).
pub(crate) const UNATTRIBUTED_IDENTITY: (&str, &str) = ("proxy", "unattributed");

/// The identity namespace of a binding's split Runs.
pub(crate) fn split_namespace(binding: &str) -> String {
    format!("binding:{binding}")
}

/// A binding to create.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingSpec {
    /// Its name (the agent-name charset).
    pub name: String,
    /// Its path prefix (`/` plus segments).
    pub prefix: String,
    /// Its upstream base URL (`http` or `https`).
    pub upstream: String,
    /// The labels its Runs carry.
    #[serde(default)]
    pub labels: Vec<(String, String)>,
    /// A `secret:NAME` reference, stored only (inert until key custody).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custodied_key: Option<String>,
}

/// A change to a binding (`None` keeps a field).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The whole next label set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<(String, String)>>,
    /// `Some(None)` clears the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custodied_key: Option<Option<String>>,
}

/// A route to put on a binding.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSpec {
    pub name: String,
    pub upstream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custodied_key: Option<String>,
}

/// One stored route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRoute {
    pub name: String,
    pub upstream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custodied_key: Option<String>,
    pub created_at: String,
}

/// One binding as a read surface shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub name: String,
    pub prefix: String,
    pub upstream: String,
    pub labels: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custodied_key: Option<String>,
    pub routes: Vec<BindingRoute>,
    pub created_at: String,
    pub updated_at: String,
}

/// A per-binding credential's class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BindingCredentialClass {
    /// `hk_ann_`: announce-scoped.
    Announce,
    /// `hk_pxy_`: proxy-access.
    ProxyAccess,
}

impl BindingCredentialClass {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            BindingCredentialClass::Announce => "announce",
            BindingCredentialClass::ProxyAccess => "proxy-access",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "announce" => Some(BindingCredentialClass::Announce),
            "proxy-access" => Some(BindingCredentialClass::ProxyAccess),
            _ => None,
        }
    }

    fn token_class(self) -> TokenClass {
        match self {
            BindingCredentialClass::Announce => TokenClass::Announce,
            BindingCredentialClass::ProxyAccess => TokenClass::ProxyAccess,
        }
    }
}

/// One per-binding credential's metadata (never a value or a hash).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingCredential {
    pub token_id: String,
    pub class: BindingCredentialClass,
    /// `active` or `revoked`.
    pub state: String,
    pub issued_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

/// A freshly issued per-binding credential: shown once, never stored.
pub struct IssuedBindingCredential {
    pub class: BindingCredentialClass,
    pub token_id: String,
    pub token: SecretString,
}

impl fmt::Debug for IssuedBindingCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedBindingCredential")
            .field("class", &self.class)
            .field("token_id", &self.token_id)
            .finish_non_exhaustive()
    }
}

/// What happened to a binding (a [`BindingEvent`]'s `change`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BindingChange {
    Created,
    Changed,
    Deleted,
    RouteAdded,
    RouteRemoved,
    CredentialIssued,
    CredentialRevoked,
}

/// One binding event (the `binding` family on the bus and in the outbox).
/// Carries a token id on a credential change, never a token or a hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingEvent {
    pub schema_version: u32,
    pub change: BindingChange,
    pub binding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_class: Option<BindingCredentialClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    pub at: String,
}

/// A refused binding write.
#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    /// A field failed validation.
    #[error("{field}: {reason}")]
    Invalid { field: String, reason: String },
    /// No binding has the name.
    #[error("no binding is named `{name}`")]
    NotFound { name: String },
    /// A binding already has the name.
    #[error("a binding named `{name}` already exists")]
    Exists { name: String },
    /// Another binding has the prefix.
    #[error("the prefix `{prefix}` is already bound by `{binding}`")]
    PrefixTaken { prefix: String, binding: String },
    /// A limit is reached.
    #[error("at most {max} {what} are allowed")]
    TooMany { what: &'static str, max: usize },
    /// No route has the name.
    #[error("binding `{binding}` has no route named `{route}`")]
    RouteNotFound { binding: String, route: String },
    /// A `/` binding while the legacy root route serves an adopted agent.
    #[error(
        "the root prefix `/` is taken by the legacy route of Run `{run_id}`, which started \
         before the shared metering proxy; it frees when that Run ends"
    )]
    LegacyRoute { run_id: String },
    /// The credential could not be minted.
    #[error("the credential could not be issued: {0}")]
    Credential(String),
    /// The store refused or failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Validate a binding name (the agent-name charset).
pub(crate) fn validate_name(field: &str, name: &str) -> Result<(), BindingError> {
    crate::domain::InstanceName::new(name)
        .map(|_| ())
        .map_err(|err| BindingError::Invalid {
            field: field.to_string(),
            reason: err.to_string(),
        })
}

/// Validate a path prefix: `/` alone, or `/` plus segments of
/// `[A-Za-z0-9._~-]`, no empty segment, no `.` or `..`, no trailing slash,
/// at most [`MAX_PREFIX_CHARS`], never under [`RESERVED_PREFIX`].
pub(crate) fn validate_prefix(prefix: &str) -> Result<(), BindingError> {
    let invalid = |reason: &str| BindingError::Invalid {
        field: "prefix".to_string(),
        reason: reason.to_string(),
    };
    if prefix == "/" {
        return Ok(());
    }
    if !prefix.starts_with('/') {
        return Err(invalid("must start with `/`"));
    }
    if prefix.len() > MAX_PREFIX_CHARS {
        return Err(invalid("must be at most 128 characters"));
    }
    for segment in prefix[1..].split('/') {
        if segment.is_empty() {
            return Err(invalid("must not have an empty segment or a trailing `/`"));
        }
        if segment == "." || segment == ".." {
            return Err(invalid("must not have a `.` or `..` segment"));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
        {
            return Err(invalid(
                "segments may use only letters, digits, `.`, `_`, `~` and `-`",
            ));
        }
    }
    if prefix.eq_ignore_ascii_case(RESERVED_PREFIX)
        || prefix
            .to_ascii_lowercase()
            .starts_with(&format!("{RESERVED_PREFIX}/"))
    {
        return Err(invalid("`/_hekma` is reserved for Hekma's own routes"));
    }
    Ok(())
}

/// Validate an upstream base URL: `http` or `https`, a host, no userinfo,
/// query or fragment.
pub(crate) fn validate_upstream(field: &str, upstream: &str) -> Result<(), BindingError> {
    let invalid = |reason: &str| BindingError::Invalid {
        field: field.to_string(),
        reason: reason.to_string(),
    };
    let lower = upstream.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("http://") {
        rest
    } else {
        return Err(invalid("must be an http:// or https:// URL"));
    };
    if upstream.contains('?') || upstream.contains('#') {
        return Err(invalid("must not carry a query or a fragment"));
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        return Err(invalid("must not carry a user name or password"));
    }
    if authority.is_empty() {
        return Err(invalid("must name a host"));
    }
    if upstream
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid("must not contain whitespace"));
    }
    match upstream.parse::<hyper::Uri>() {
        Ok(uri) if uri.host().is_some_and(|host| !host.is_empty()) => Ok(()),
        _ => Err(invalid("is not a valid URL")),
    }
}

/// Validate a custodied-key reference: `secret:NAME`.
pub(crate) fn validate_custodied_key(field: &str, key: &str) -> Result<(), BindingError> {
    let name = crate::domain::config::secret_name(key).ok_or_else(|| BindingError::Invalid {
        field: field.to_string(),
        reason: "must be a `secret:NAME` reference; a key value is never stored".to_string(),
    })?;
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(BindingError::Invalid {
            field: field.to_string(),
            reason: "the secret name may use only letters, digits, `_`, `-` and `.`".to_string(),
        });
    }
    Ok(())
}

/// Validate a label set (each label like any label; no key twice).
pub(crate) fn validate_labels(labels: &[(String, String)]) -> Result<(), BindingError> {
    let mut seen = std::collections::BTreeSet::new();
    for (key, value) in labels {
        super::labels::validate_label_write(key, value).map_err(|reason| {
            BindingError::Invalid {
                field: "label".to_string(),
                reason,
            }
        })?;
        if !seen.insert(key.as_str()) {
            return Err(BindingError::Invalid {
                field: "label".to_string(),
                reason: format!("label key `{key}` is given twice"),
            });
        }
    }
    Ok(())
}

fn sorted(mut labels: Vec<(String, String)>) -> Vec<(String, String)> {
    labels.sort();
    labels
}

/// The read view of a stored binding with its routes.
pub(crate) fn view(
    store: &crate::store::SqliteStore,
    row: BindingRow,
) -> Result<Binding, StoreError> {
    let routes = store
        .binding_routes(&row.name)?
        .into_iter()
        .map(|route| BindingRoute {
            name: route.route,
            upstream: route.upstream,
            custodied_key: route.custodied_key,
            created_at: route.created_at,
        })
        .collect();
    Ok(Binding {
        name: row.name,
        prefix: row.prefix,
        upstream: row.upstream,
        labels: row.labels,
        custodied_key: row.custodied_key,
        routes,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// The binding table the snapshot carries.
pub(crate) fn table_of(rows: &[BindingRow]) -> BindingTable {
    BindingTable::new(
        rows.iter()
            .map(|row| BindingEntry {
                name: row.name.clone(),
                prefix: row.prefix.clone(),
                upstream: std::sync::Arc::from(row.upstream.as_str()),
                labels: row.labels.clone(),
            })
            .collect(),
    )
}

/// What a committed binding job hands back: its view (none after a
/// delete), its committed events in commit order, and an issued credential.
#[derive(Debug, Default)]
pub struct BindingOutcome {
    pub binding: Option<Binding>,
    pub credential: Option<IssuedBindingCredential>,
    pub notes: Vec<String>,
    #[doc(hidden)]
    pub(crate) events: Vec<(EngineEvent, OutboxAppend)>,
}

/// The job context every binding job shares.
pub(crate) struct BindingJob<'a, 'b> {
    pub ctx: &'a mut WriterCtx<'b>,
    pub publisher: &'a std::sync::Arc<SnapshotPublisher>,
}

impl BindingJob<'_, '_> {
    fn event(
        &mut self,
        outcome: &mut BindingOutcome,
        change: BindingChange,
        binding: &str,
        detail: EventDetail,
        at: &str,
    ) -> Result<(), StoreError> {
        let event = EngineEvent::Binding(Box::new(BindingEvent {
            schema_version: BINDING_EVENT_SCHEMA_VERSION,
            change,
            binding: binding.to_string(),
            prefix: detail.prefix,
            route: detail.route,
            credential_class: detail.class,
            token_id: detail.token_id,
            at: at.to_string(),
        }));
        let append = self.ctx.store().append_outbox(&event, OUTBOX_MAX_BYTES)?;
        outcome.events.push((event, append));
        Ok(())
    }

    fn publish_table(&mut self) -> Result<(), StoreError> {
        let table = table_of(&self.ctx.store().bindings()?);
        let publisher = std::sync::Arc::clone(self.publisher);
        self.ctx
            .after_commit(move || publisher.publish_bindings(table));
        Ok(())
    }

    fn commit_runs(
        &mut self,
        outcome: &mut BindingOutcome,
        changes: RunChanges,
    ) -> Result<(), StoreError> {
        outcome.notes.extend(changes.notes.iter().cloned());
        let committed: Vec<CommittedRunEvent> = ops::commit(self.ctx, self.publisher, changes)?;
        outcome.events.extend(committed);
        Ok(())
    }
}

#[derive(Default)]
struct EventDetail {
    prefix: Option<String>,
    route: Option<String>,
    class: Option<BindingCredentialClass>,
    token_id: Option<String>,
}

fn check_prefix_free(
    store: &crate::store::SqliteStore,
    prefix: &str,
    except: Option<&str>,
    legacy_root: Option<&str>,
) -> Result<(), BindingError> {
    if prefix == "/" {
        if let Some(run_id) = legacy_root {
            return Err(BindingError::LegacyRoute {
                run_id: run_id.to_string(),
            });
        }
    }
    if let Some(other) = store
        .bindings()?
        .into_iter()
        .find(|row| row.prefix == prefix && Some(row.name.as_str()) != except)
    {
        return Err(BindingError::PrefixTaken {
            prefix: prefix.to_string(),
            binding: other.name,
        });
    }
    Ok(())
}

/// Validate a whole spec (pure).
pub(crate) fn validate_spec(spec: &BindingSpec) -> Result<(), BindingError> {
    validate_name("name", &spec.name)?;
    validate_prefix(&spec.prefix)?;
    validate_upstream("upstream", &spec.upstream)?;
    validate_labels(&spec.labels)?;
    if let Some(key) = &spec.custodied_key {
        validate_custodied_key("custodied_key", key)?;
    }
    Ok(())
}

/// Create a binding (the writer job).
pub(crate) fn create(
    job: &mut BindingJob<'_, '_>,
    spec: BindingSpec,
    legacy_root: Option<String>,
    at: &str,
) -> Result<BindingOutcome, BindingError> {
    validate_spec(&spec)?;
    let store = job.ctx.store();
    if store.binding(&spec.name)?.is_some() {
        return Err(BindingError::Exists { name: spec.name });
    }
    if store.bindings()?.len() >= MAX_BINDINGS {
        return Err(BindingError::TooMany {
            what: "bindings",
            max: MAX_BINDINGS,
        });
    }
    check_prefix_free(store, &spec.prefix, None, legacy_root.as_deref())?;
    let row = BindingRow {
        name: spec.name.clone(),
        prefix: spec.prefix.clone(),
        upstream: spec.upstream,
        custodied_key: spec.custodied_key,
        labels: sorted(spec.labels),
        created_at: at.to_string(),
        updated_at: at.to_string(),
    };
    store.put_binding(&row)?;
    let mut outcome = BindingOutcome::default();
    if row.custodied_key.is_some() {
        outcome.notes.push(custody_note());
    }
    job.event(
        &mut outcome,
        BindingChange::Created,
        &row.name,
        EventDetail {
            prefix: Some(row.prefix.clone()),
            ..EventDetail::default()
        },
        at,
    )?;
    job.publish_table()?;
    outcome.binding = Some(view(job.ctx.store(), row)?);
    Ok(outcome)
}

/// The note a stored custodied-key reference carries.
pub(crate) fn custody_note() -> String {
    "A custodied key is stored as a reference only; Hekma does not use it until key custody \
     ships."
        .to_string()
}

/// Change a binding (the writer job): a prefix or upstream change keeps its
/// Runs; a label change re-resolves them in the same job.
pub(crate) fn update(
    job: &mut BindingJob<'_, '_>,
    name: &str,
    patch: BindingPatch,
    legacy_root: Option<String>,
    at: &str,
) -> Result<BindingOutcome, BindingError> {
    let store = job.ctx.store();
    let Some(mut row) = store.binding(name)? else {
        return Err(BindingError::NotFound {
            name: name.to_string(),
        });
    };
    if let Some(prefix) = &patch.prefix {
        validate_prefix(prefix)?;
        check_prefix_free(store, prefix, Some(name), legacy_root.as_deref())?;
        row.prefix = prefix.clone();
    }
    if let Some(upstream) = &patch.upstream {
        validate_upstream("upstream", upstream)?;
        row.upstream = upstream.clone();
    }
    if let Some(key) = &patch.custodied_key {
        if let Some(key) = key {
            validate_custodied_key("custodied_key", key)?;
        }
        row.custodied_key = key.clone();
    }
    let labels_changed = match &patch.labels {
        Some(labels) => {
            validate_labels(labels)?;
            let next = sorted(labels.clone());
            let changed = next != row.labels;
            row.labels = next;
            changed
        }
        None => false,
    };
    row.updated_at = at.to_string();
    store.put_binding(&row)?;
    let mut outcome = BindingOutcome::default();
    if patch.custodied_key.as_ref().is_some_and(Option::is_some) {
        outcome.notes.push(custody_note());
    }
    job.event(
        &mut outcome,
        BindingChange::Changed,
        name,
        EventDetail {
            prefix: Some(row.prefix.clone()),
            ..EventDetail::default()
        },
        at,
    )?;
    if labels_changed {
        let mut changes = RunChanges::default();
        for run in store.binding_open_runs(name)? {
            changes.extend(ops::sync_binding_labels(store, &run, &row.labels, at)?);
        }
        job.commit_runs(&mut outcome, changes)?;
    }
    job.publish_table()?;
    outcome.binding = Some(view(job.ctx.store(), row)?);
    Ok(outcome)
}

/// Delete a binding (the writer job): its Run and its split Runs end with
/// cause `binding-removed`.
pub(crate) fn delete(
    job: &mut BindingJob<'_, '_>,
    name: &str,
    at: &str,
) -> Result<BindingOutcome, BindingError> {
    let store = job.ctx.store();
    if store.binding(name)?.is_none() {
        return Err(BindingError::NotFound {
            name: name.to_string(),
        });
    }
    let mut changes = RunChanges::default();
    for run in store.binding_open_runs(name)? {
        if let Some(record) = store.run_record(&run)? {
            if !record.is_ended() {
                let anomalies = record.anomalies.clone();
                ops::move_to(
                    store,
                    &mut changes,
                    record,
                    RunStatus::Ended,
                    anomalies,
                    Some("binding-removed".to_string()),
                    at,
                )?;
            }
        }
    }
    store.delete_binding(name)?;
    let mut outcome = BindingOutcome::default();
    job.event(
        &mut outcome,
        BindingChange::Deleted,
        name,
        EventDetail::default(),
        at,
    )?;
    job.commit_runs(&mut outcome, changes)?;
    job.publish_table()?;
    Ok(outcome)
}

/// Put a route on a binding (the writer job).
pub(crate) fn put_route(
    job: &mut BindingJob<'_, '_>,
    binding: &str,
    route: RouteSpec,
    at: &str,
) -> Result<BindingOutcome, BindingError> {
    validate_name("route", &route.name)?;
    validate_upstream("upstream", &route.upstream)?;
    if let Some(key) = &route.custodied_key {
        validate_custodied_key("custodied_key", key)?;
    }
    let store = job.ctx.store();
    let Some(row) = store.binding(binding)? else {
        return Err(BindingError::NotFound {
            name: binding.to_string(),
        });
    };
    let routes = store.binding_routes(binding)?;
    let existing = routes.iter().find(|r| r.route == route.name);
    if existing.is_none() && routes.len() >= MAX_BINDING_ROUTES {
        return Err(BindingError::TooMany {
            what: "routes per binding",
            max: MAX_BINDING_ROUTES,
        });
    }
    store.put_binding_route(
        binding,
        &RouteRow {
            route: route.name.clone(),
            upstream: route.upstream,
            custodied_key: route.custodied_key.clone(),
            created_at: existing
                .map(|r| r.created_at.clone())
                .unwrap_or_else(|| at.to_string()),
        },
    )?;
    let mut outcome = BindingOutcome::default();
    if route.custodied_key.is_some() {
        outcome.notes.push(custody_note());
    }
    job.event(
        &mut outcome,
        BindingChange::RouteAdded,
        binding,
        EventDetail {
            route: Some(route.name),
            ..EventDetail::default()
        },
        at,
    )?;
    outcome.binding = Some(view(job.ctx.store(), row)?);
    Ok(outcome)
}

/// Remove a route (the writer job).
pub(crate) fn remove_route(
    job: &mut BindingJob<'_, '_>,
    binding: &str,
    route: &str,
    at: &str,
) -> Result<BindingOutcome, BindingError> {
    let store = job.ctx.store();
    let Some(row) = store.binding(binding)? else {
        return Err(BindingError::NotFound {
            name: binding.to_string(),
        });
    };
    if !store.remove_binding_route(binding, route)? {
        return Err(BindingError::RouteNotFound {
            binding: binding.to_string(),
            route: route.to_string(),
        });
    }
    let mut outcome = BindingOutcome::default();
    job.event(
        &mut outcome,
        BindingChange::RouteRemoved,
        binding,
        EventDetail {
            route: Some(route.to_string()),
            ..EventDetail::default()
        },
        at,
    )?;
    outcome.binding = Some(view(job.ctx.store(), row)?);
    Ok(outcome)
}

/// Issue a per-binding credential (the writer job): a second issue of a
/// class revokes the first in the same job. `minted` was minted by the
/// caller (the cleartext never enters the job; only its hash and id).
pub(crate) fn issue_credential(
    job: &mut BindingJob<'_, '_>,
    binding: &str,
    class: BindingCredentialClass,
    stub: credential::TokenStub,
    at: &str,
) -> Result<(BindingOutcome, String), BindingError> {
    let store = job.ctx.store();
    if store.binding(binding)?.is_none() {
        return Err(BindingError::NotFound {
            name: binding.to_string(),
        });
    }
    let mut outcome = BindingOutcome::default();
    if let Some(previous) =
        store.revoke_binding_credential(binding, class.as_str(), at, "rotated")?
    {
        job.event(
            &mut outcome,
            BindingChange::CredentialRevoked,
            binding,
            EventDetail {
                class: Some(class),
                token_id: Some(previous),
                ..EventDetail::default()
            },
            at,
        )?;
    }
    let token_id = stub.hash.token_id();
    job.ctx.store().insert_binding_credential(
        binding,
        &stub.hash,
        &token_id,
        class.as_str(),
        at,
    )?;
    job.event(
        &mut outcome,
        BindingChange::CredentialIssued,
        binding,
        EventDetail {
            class: Some(class),
            token_id: Some(token_id.clone()),
            ..EventDetail::default()
        },
        at,
    )?;
    Ok((outcome, token_id))
}

/// Revoke a binding's active credential of `class` (the writer job). No
/// active credential is a no-op with a note.
pub(crate) fn revoke_credential(
    job: &mut BindingJob<'_, '_>,
    binding: &str,
    class: BindingCredentialClass,
    at: &str,
) -> Result<BindingOutcome, BindingError> {
    let store = job.ctx.store();
    if store.binding(binding)?.is_none() {
        return Err(BindingError::NotFound {
            name: binding.to_string(),
        });
    }
    let mut outcome = BindingOutcome::default();
    match store.revoke_binding_credential(binding, class.as_str(), at, "operator")? {
        Some(token_id) => job.event(
            &mut outcome,
            BindingChange::CredentialRevoked,
            binding,
            EventDetail {
                class: Some(class),
                token_id: Some(token_id),
                ..EventDetail::default()
            },
            at,
        )?,
        None => outcome.notes.push(format!(
            "binding `{binding}` has no active {} credential; nothing was revoked",
            class.as_str()
        )),
    }
    Ok(outcome)
}

/// Mint a per-binding credential (outside the job).
pub(crate) fn mint(class: BindingCredentialClass) -> Result<credential::MintedToken, BindingError> {
    credential::mint(class.token_class()).map_err(|err| BindingError::Credential(err.to_string()))
}

/// A binding's credentials (metadata).
pub(crate) fn credentials(
    store: &crate::store::SqliteStore,
    binding: &str,
) -> Result<Vec<BindingCredential>, BindingError> {
    if store.binding(binding)?.is_none() {
        return Err(BindingError::NotFound {
            name: binding.to_string(),
        });
    }
    Ok(store
        .binding_credentials(binding)?
        .into_iter()
        .filter_map(|row| {
            Some(BindingCredential {
                token_id: row.token_id,
                class: BindingCredentialClass::from_wire(&row.class)?,
                state: row.state,
                issued_at: row.issued_at,
                revoked_at: row.revoked_at,
            })
        })
        .collect())
}

/// Every binding by name, after `after` (keyset), at most `limit`.
pub(crate) fn list(
    store: &crate::store::SqliteStore,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<Binding>, StoreError> {
    store
        .bindings()?
        .into_iter()
        .filter(|row| after.is_none_or(|after| row.name.as_str() > after))
        .take(limit.clamp(1, MAX_BINDINGS))
        .map(|row| view(store, row))
        .collect()
}

#[cfg(test)]
mod tests;
