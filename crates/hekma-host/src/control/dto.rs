//! The Control API's own DTOs (story 17-3) — the AD-2 amendment's "its own
//! Control-API types" rule: the host defines its WIRE shapes here and
//! projects the engine's already-versioned payload structs VERBATIM inside
//! them, never re-deriving engine internals.
//!
//! Versioning discipline (AD-14 / the content-family rule): every payload is
//! stamped `schema_version`. Two stamping flavors live side by side, each
//! honest about its family:
//!
//! * Payloads that WRAP an engine document (`fleet`, the show/status/config
//!   reads) reuse that document's OWN schema version — the number the CLI's
//!   identical wire shape already carries (`FLEET_SCHEMA_VERSION` = 2, the
//!   config-get family's 1). The Control API does not mint a second version
//!   space for a shape the engine already versions.
//! * Payloads that are Control-API originals (`/v0/version`, the per-instance
//!   status/capabilities wrappers — shapes no CLI renders) carry
//!   [`CONTROL_API_SCHEMA_VERSION`], minted here at 1 under the
//!   unstable-`v0` marker (`/v0/version` self-describes it; additive-only
//!   evolution until a second Host validates the surface).

use hekma_engine::{AgentInstance, EffectiveCapabilities, RestartPolicy};
use serde::{Deserialize, Serialize};

/// The Control-API-original document schema version (the version + status +
/// capabilities + config-write wrappers minted by this module). v0's first
/// stamp; additive fields only while `stability` reads `unstable`.
pub(crate) const CONTROL_API_SCHEMA_VERSION: u32 = 1;

/// The `/v0` path prefix — ONE version segment for the whole surface (the
/// Adapter-Contract discipline: additive routes/fields only until a second
/// Host validates).
pub(crate) const API_VERSION: &str = "v0";

/// The stability marker every self-description carries. v0 is UNSTABLE by
/// definition (the frozen intent): the surface freezes only after a second
/// Host validates it.
pub(crate) const STABILITY: &str = "unstable";

/// The API identity string `/v0/version` self-describes with.
pub(crate) const API_ID: &str = "hekma-control-api";

/// The config document's schema version — MIRRORED from the CLI's
/// `config get --json` family (`CONFIG_GET_SCHEMA_VERSION` in
/// `crates/hekma/src/cli/agent.rs`): the Control API serves the SAME wire
/// shape (`{schema_version, entries: [{key, value, source, unvalidated}]}`),
/// so it carries the SAME version. The constant is re-declared here (the CLI
/// crate is not a host dependency — AD-2), and the two declarations are held
/// equal by the SOURCE-SCAN pin in `scripts/test_automation.py`
/// (`test_config_version_mirror_across_the_cli_and_host_crates`) — the host
/// crate cannot see the CLI crate's constant at compile time, so the pin
/// reads both source files and fails the automation run on drift. The unit
/// test below additionally pins THIS crate's value so a local change cannot
/// slip past unnoticed.
pub(crate) const CONFIG_GET_SCHEMA_VERSION: u32 = 1;

/// `GET /v0/version` — the self-description every client reads first.
#[derive(Serialize)]
pub(crate) struct VersionDocument {
    /// The API identity ([`API_ID`]).
    pub api: &'static str,
    /// The path-version segment ([`API_VERSION`]) — the `/v0` the routes
    /// live under.
    pub version: &'static str,
    /// The stability marker ([`STABILITY`]) — `"unstable"` for v0.
    pub stability: &'static str,
    /// The Control-API-original document schema version.
    pub schema_version: u32,
    /// The HOST crate's version — the binary a Host is talking to. A
    /// top-level field: it describes THIS surface's build, not the engine's
    /// payload families.
    pub host_version: &'static str,
    /// The engine-side wire-schema versions this build projects.
    pub engine: EngineVersions,
    /// The engine's log-capture posture (story 17-4, review finding 21):
    /// `full` | `metadata` | `off` — the self-description that tells a Host
    /// what `output.log` holds and which capture surfaces serve, without a
    /// failed round-trip (and the honest bound on what "off" announces:
    /// the version document, the typed read refusals, and the Fleet's
    /// `capture-off` gap cell).
    pub log_capture: &'static str,
}

/// The engine-side wire-schema versions [`VersionDocument`] carries — the
/// SAME constants the engine's own payloads stamp, read from
/// `hekma_engine`'s public re-exports (never re-hardcoded).
#[derive(Serialize)]
pub(crate) struct EngineVersions {
    /// The instance-log event family's version (`EVENT_SCHEMA_VERSION`).
    pub event: u32,
    /// The Fleet document family's version (`FLEET_SCHEMA_VERSION`).
    pub fleet: u32,
    /// The usage-ledger family's version (`USAGE_SCHEMA_VERSION`).
    pub usage: u32,
    /// The budget-breach family's version (`BUDGET_SCHEMA_VERSION`).
    pub budget: u32,
    /// The instance-log line family's version (`LOG_SCHEMA_VERSION`).
    pub log: u32,
}

/// `GET /v0/instances/{name}` — the single-instance read. The SAME shape
/// `kt agent show <name> --json` renders: the Fleet document family's
/// schema version over one [`FleetEntry`].
#[derive(Serialize)]
pub(crate) struct ShowDocument {
    /// The Fleet document schema version (`FLEET_SCHEMA_VERSION`) — the SAME
    /// stamp `kt agent show --json` carries for the same shape.
    pub schema_version: u32,
    /// The instance's Fleet row (budget/usage/metering included — the v0
    /// usage/budget projection: no separate routes, the Fleet row IS it).
    pub instance: hekma_engine::FleetEntry,
}

/// `GET /v0/instances/{name}/status` — the runtime status read.
#[derive(Serialize)]
pub(crate) struct StatusDocument {
    /// The document family stamp — ONE schema-version discipline for the
    /// whole 0.x API ([`CONTROL_API_SCHEMA_VERSION`]); the status family has
    /// no separate counter.
    pub schema_version: u32,
    /// The status itself.
    pub status: StatusDetail,
}

/// The status payload — [`hekma_engine::InstanceStatus`]'s fields rendered
/// one-for-one (that struct is a host-facing read, not `Serialize`, so the
/// wire shape is minted here; the field set mirrors it exactly).
#[derive(Serialize)]
pub(crate) struct StatusDetail {
    /// The instance's registry row.
    pub instance: AgentInstance,
    /// The effective per-instance Restart Policy (AD-15).
    pub restart_policy: RestartPolicy,
    /// The consecutive-failure restart count (0 when never restarted).
    pub restart_count: u32,
    /// The last-known failure cause, for a `failed` instance.
    pub failed_cause: Option<String>,
}

impl From<hekma_engine::InstanceStatus> for StatusDetail {
    fn from(status: hekma_engine::InstanceStatus) -> Self {
        StatusDetail {
            instance: status.instance,
            restart_policy: status.restart_policy,
            restart_count: status.restart_count,
            failed_cause: status.failed_cause,
        }
    }
}

/// `GET /v0/instances/{name}/capabilities` — the effective Capability
/// Declaration projected onto the current OS.
#[derive(Serialize)]
pub(crate) struct CapabilitiesDocument {
    /// The capabilities document schema version (Control-API original).
    pub schema_version: u32,
    /// The engine's own versioned projection, verbatim.
    pub capabilities: EffectiveCapabilities,
}

/// One rendered config leaf — the EXACT leaf shape the CLI's
/// `config get --json` document carries (`{key, value, source, unvalidated}`).
/// `value` is the single-display-path rendering, which MASKS a `secret:` leaf
/// by construction (the API projects NO reveal: `reveal_secrets` is never
/// called — FR-41's hard line).
#[derive(Serialize)]
pub(crate) struct ConfigLeaf {
    /// The dotted leaf key.
    pub key: String,
    /// The rendered winning value (masked for secrets, like the CLI table).
    pub value: String,
    /// The winning source layer's kebab-case label.
    pub source: String,
    /// `true` for an `agent.*` pass-through leaf (skipped known-key validation).
    pub unvalidated: bool,
}

/// `GET /v0/instances/{name}/config` — the effective config, the SAME
/// document `config get --json` renders ([`CONFIG_GET_SCHEMA_VERSION`]).
#[derive(Serialize)]
pub(crate) struct ConfigDocument {
    /// The config-get family's schema version.
    pub schema_version: u32,
    /// The resolved leaves, key-ordered.
    pub entries: Vec<ConfigLeaf>,
}

/// `PUT /v0/instances/{name}/config` — the write acknowledgment: the
/// post-write config read-back (the GET document's shape) PLUS the
/// zero-or-more WARN-ONLY steering warnings the engine returned (AI-18:
/// a warning that only the host saw would be a silent degradation — they
/// ride the response body).
#[derive(Serialize)]
pub(crate) struct ConfigPutDocument {
    /// The config-get family's schema version (the read-back's shape).
    pub schema_version: u32,
    /// The effective config AFTER the write.
    pub entries: Vec<ConfigLeaf>,
    /// The WARN-ONLY steering warnings (empty on the common path).
    pub warnings: Vec<String>,
}

/// `POST /v0/instances` — the registration body. EXACTLY ONE adapter source:
/// `kind` (a native, compiled-in kind) or `manifest` (an adapter.toml
/// directory/file path) — both, or neither, is a 400 (`invalid_body`), the
/// body-level mirror of the CLI's mutually-exclusive `--kind`/`--manifest`
/// flags. Unknown fields are TOLERATED (serde default), the 0.x additive
/// posture: a newer Host may send fields this build does not know.
#[derive(Deserialize)]
pub(crate) struct RegisterRequest {
    /// The Fleet-unique instance name (`^[a-z0-9][a-z0-9_-]*$`).
    pub name: String,
    /// A native adapter kind (e.g. `mock`). Mutually exclusive with `manifest`.
    pub kind: Option<String>,
    /// A manifest adapter path (directory or direct `adapter.toml`). Mutually
    /// exclusive with `kind`.
    pub manifest: Option<String>,
}

/// `POST /v0/instances/{name}/stop` — the optional stop body. ABSENT (an
/// empty request body) is the engine default window; `window_secs` bounds
/// the graceful ladder explicitly ([`crate::MIN_STOP_WINDOW_SECS`]..
/// [`crate::MAX_STOP_WINDOW_SECS`] — the same bounds the `--stop-window`
/// flag enforces; out-of-bounds is a 400).
#[derive(Deserialize, Default)]
pub(crate) struct StopRequest {
    /// The graceful stop window in whole seconds (`None` = engine default).
    pub window_secs: Option<u64>,
}

/// `POST /v0/instances/{name}/drain` (story 18-4) — the DRAIN composite's
/// body: the optional bound override in milliseconds (`None`/absent body =
/// the manifest `[drain].timeout_ms` tier, else the engine's 30 s default —
/// the three-tier rule). Bounds-checked in the route (`1..=600_000`, the
/// same ceiling the adapter contract pins — out-of-bounds is a 400), the
/// same shape as `window_secs` above.
#[derive(Deserialize, Default)]
pub(crate) struct DrainRequest {
    /// The drain bound override in milliseconds (`None` = the manifest tier,
    /// else the engine default).
    pub timeout_ms: Option<u64>,
}

/// The `POST /v0/instances/{name}/drain` response (story 18-4): the
/// post-drain read-back PLUS the composite's durable outcome. Both outcomes
/// are a `200` — the drain's last step (the stop) is unconditional, so
/// `completed: false` (the bound burned first) is STILL a stopped agent,
/// reported honestly rather than as an error.
#[derive(Serialize)]
pub(crate) struct DrainDocument {
    /// The Fleet document family's version — the same stamp the show read
    /// carries for the same instance shape.
    pub schema_version: u32,
    /// The instance's post-drain Fleet row (`stopped` in both outcomes).
    pub instance: hekma_engine::FleetEntry,
    /// Whether the bounded wait COMPLETED before the bound (or no wait was
    /// declared). `false` = the bound burned first; the stop still happened.
    pub completed: bool,
    /// The measured wall clock of the whole bounded phase (the pause
    /// command included), in milliseconds — as-spent, honest.
    pub waited_ms: u64,
}

/// `PUT /v0/instances/{name}/config` — the write body: one dotted key and
/// its string value, the body-form of `config set <name> <key> <value>`.
#[derive(Deserialize)]
pub(crate) struct ConfigPutRequest {
    /// The dotted config key to set.
    pub key: String,
    /// The value to set (string form; the engine validates per key).
    pub value: String,
}

/// `GET /v0/export` — the outbox-pull body (story 17-4): the engine's
/// [`ExportBatch`](hekma_engine::ExportBatch) served VERBATIM (the
/// projection rule above — the host wraps nothing it can reuse). The batch
/// is already `schema_version`-stamped (`OUTBOX_SCHEMA_VERSION`),
/// single-ordered across event families, cursor-bearing, and
/// `torn_tail_skipped`-honest — re-wrapping it would mint a second version
/// space for a shape the engine already versions.
///
/// The ack document's mirror ([`ExportAckDocument`]) is Control-API-original
/// (no CLI renders it), so it alone carries [`CONTROL_API_SCHEMA_VERSION`].
pub(crate) type ExportBody = hekma_engine::ExportBatch;

/// `POST /v0/export/ack` — the request body: the highest seq the caller has
/// DURABLY stored. The engine monotonically advances to
/// `max(current, given)`.
#[derive(Deserialize)]
pub(crate) struct ExportAckRequest {
    /// The highest exported seq the caller has durably persisted.
    pub seq: u64,
}

/// `POST /v0/export/ack` — the response body: the ack cursor AS PERSISTED
/// (the monotonic `max(current, given)` — so a re-ack reads back its own
/// idempotence instead of guessing). Control-API-original, stamped
/// [`CONTROL_API_SCHEMA_VERSION`].
#[derive(Serialize)]
pub(crate) struct ExportAckDocument {
    /// The Control-API-original family version.
    pub schema_version: u32,
    /// The ack cursor now durable in the engine's `outbox-ack.json`.
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mirror pin: the CLI's `config get --json` family is version 1.
    /// If the CLI's constant ever moves, this pin goes red in the SAME
    /// review that moves it (the Control API serves the SAME shape — the
    /// versions must move together, never silently).
    #[test]
    fn the_config_document_mirrors_the_cli_config_get_family_version() {
        assert_eq!(CONFIG_GET_SCHEMA_VERSION, 1);
    }

    #[test]
    fn the_version_document_self_describes_the_unstable_v0_surface() {
        let document = VersionDocument {
            api: API_ID,
            version: API_VERSION,
            stability: STABILITY,
            schema_version: CONTROL_API_SCHEMA_VERSION,
            host_version: env!("CARGO_PKG_VERSION"),
            engine: EngineVersions {
                event: hekma_engine::EVENT_SCHEMA_VERSION,
                fleet: hekma_engine::FLEET_SCHEMA_VERSION,
                usage: hekma_engine::USAGE_SCHEMA_VERSION,
                budget: hekma_engine::BUDGET_SCHEMA_VERSION,
                log: hekma_engine::LOG_SCHEMA_VERSION,
            },
            log_capture: hekma_engine::LogCaptureMode::Full.as_str(),
        };
        let json = serde_json::to_value(&document).unwrap();
        assert_eq!(json["api"], "hekma-control-api");
        assert_eq!(json["version"], "v0");
        assert_eq!(json["stability"], "unstable");
        assert_eq!(json["schema_version"], 1);
        // The HOST version is a TOP-LEVEL field — it describes this surface's
        // build, not an engine payload family (review item 12).
        assert_eq!(json["host_version"], env!("CARGO_PKG_VERSION"));
        assert!(json["engine"].get("host_version").is_none());
        // The engine family versions ride through the engine's OWN constants
        // (spot-pinned: the Fleet family is 2 — `FLEET_SCHEMA_VERSION`).
        assert_eq!(json["engine"]["fleet"], hekma_engine::FLEET_SCHEMA_VERSION);
        assert_eq!(json["engine"]["fleet"], 2);
    }

    #[test]
    fn the_status_document_carries_the_instance_status_fields_one_for_one() {
        let json = serde_json::to_value(&StatusDetail {
            instance: sample_instance(),
            restart_policy: RestartPolicy::OnFailure,
            restart_count: 2,
            failed_cause: Some("crashed: exit 1".into()),
        })
        .unwrap();
        assert_eq!(json["instance"]["name"], "probe");
        assert_eq!(json["restart_policy"], "on-failure");
        assert_eq!(json["restart_count"], 2);
        assert_eq!(json["failed_cause"], "crashed: exit 1");
        let document = StatusDocument {
            schema_version: CONTROL_API_SCHEMA_VERSION,
            status: StatusDetail {
                instance: sample_instance(),
                restart_policy: RestartPolicy::Never,
                restart_count: 0,
                failed_cause: None,
            },
        };
        let json = serde_json::to_value(&document).unwrap();
        assert_eq!(json["schema_version"], CONTROL_API_SCHEMA_VERSION);
        assert_eq!(json["status"]["failed_cause"], serde_json::Value::Null);
    }

    #[test]
    fn the_status_wire_mirrors_every_instance_status_field_exhaustively() {
        // The wire-mirror pin (review item 28): `StatusDetail` must render
        // EVERY field of `hekma_engine::InstanceStatus` — a field the engine
        // ADDS must never silently drop from the wire. The pin is the
        // exhaustive destructuring below: with NO `..` rest pattern, a new
        // engine field breaks THIS test's compile in the same review that
        // adds it, forcing the wire projection to be revisited before it can
        // ship. Each destructured field is then asserted to land in the
        // serialized document, so a DROPPED projection arm fails at runtime
        // too.
        let status = hekma_engine::InstanceStatus {
            instance: sample_instance(),
            restart_policy: RestartPolicy::OnFailure,
            restart_count: 3,
            failed_cause: Some("crashed: exit 1".into()),
        };
        // Exhaustive destructuring: compile-time exhaustiveness IS the pin.
        let hekma_engine::InstanceStatus {
            instance,
            restart_policy,
            restart_count,
            failed_cause,
        } = &status;
        let document = StatusDetail {
            instance: instance.clone(),
            restart_policy: *restart_policy,
            restart_count: *restart_count,
            failed_cause: failed_cause.clone(),
        };
        assert_eq!(document.instance.name, instance.name);
        assert_eq!(document.restart_policy, *restart_policy);
        assert_eq!(document.restart_count, *restart_count);
        assert_eq!(document.failed_cause, *failed_cause);
        let json = serde_json::to_value(&document).unwrap();
        // And every one of them is present on the wire.
        assert!(json.get("instance").is_some());
        assert!(json.get("restart_policy").is_some());
        assert!(json.get("restart_count").is_some());
        assert!(json.get("failed_cause").is_some());
    }

    #[test]
    fn the_register_body_round_trips_and_the_exactly_one_of_rule_is_enforceable() {
        let kind_only: RegisterRequest =
            serde_json::from_str(r#"{"name": "probe", "kind": "mock"}"#).unwrap();
        assert_eq!(kind_only.kind.as_deref(), Some("mock"));
        assert_eq!(kind_only.manifest, None);

        let manifest_only: RegisterRequest = serde_json::from_str(
            r#"{"name": "probe", "manifest": "/adapters/echo", "future": true}"#,
        )
        .unwrap();
        // The additive posture: an unknown field does not reject the body.
        assert!(manifest_only.kind.is_none());
        assert_eq!(manifest_only.manifest.as_deref(), Some("/adapters/echo"));

        let neither: RegisterRequest = serde_json::from_str(r#"{"name": "probe"}"#).unwrap();
        assert!(neither.kind.is_none() && neither.manifest.is_none());
        let both: RegisterRequest =
            serde_json::from_str(r#"{"name": "probe", "kind": "mock", "manifest": "/x"}"#).unwrap();
        assert!(both.kind.is_some() && both.manifest.is_some());
    }

    #[test]
    fn the_stop_body_defaults_to_the_engine_window_when_absent() {
        let empty: StopRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.window_secs, None);
        let explicit: StopRequest = serde_json::from_str(r#"{"window_secs": 5}"#).unwrap();
        assert_eq!(explicit.window_secs, Some(5));
    }

    #[test]
    fn a_stop_body_that_is_not_an_object_is_rejected() {
        assert!(serde_json::from_str::<StopRequest>(r#"{"window_secs": "soon"}"#).is_err());
        assert!(serde_json::from_str::<StopRequest>(r#"{"window_secs": -1}"#).is_err());
    }

    #[test]
    fn the_drain_body_defaults_to_the_lower_tier_when_absent() {
        // Story 18-4: `{}` / no body / `{"timeout_ms":null}` all select the
        // manifest tier (then the engine default); an explicit value rides
        // as the override.
        let empty: DrainRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.timeout_ms, None);
        let explicit: DrainRequest = serde_json::from_str(r#"{"timeout_ms": 1500}"#).unwrap();
        assert_eq!(explicit.timeout_ms, Some(1500));
        assert!(serde_json::from_str::<DrainRequest>(r#"{"timeout_ms": "soon"}"#).is_err());
        assert!(serde_json::from_str::<DrainRequest>(r#"{"timeout_ms": -1}"#).is_err());
    }

    /// A minimal registry row for the serialization pins (the wire shape is
    /// the engine's; only the wrapper's stamp is minted here).
    fn sample_instance() -> AgentInstance {
        let json = serde_json::json!({
            "name": "probe", "kind": "mock", "state": "registered",
            "agent_home": "/state/agents/probe",
            "created_at": "2026-10-19T00:00:00Z", "updated_at": "2026-10-19T00:00:00Z"
        });
        serde_json::from_value(json).expect("sample instance")
    }
}
