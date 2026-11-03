//! The engine-error wire codec (story 21-4): the Control API carries the
//! engine's TYPED error on the wire, so a client that links the engine (the
//! `hekma` CLI) decodes it back into the engine's own error value and renders
//! it exactly as it would have rendered an in-process failure — same text,
//! same exit code.
//!
//! The encoded object rides the error envelope as the additive
//! `error.engine_error` field:
//!
//! ```json
//! {"schema_version":1,"family":"engine","variant":"NotFound","fields":{"name":"x"}}
//! ```
//!
//! * `family` is `engine` ([`EngineError`]), `registry` ([`RegistryError`]) or
//!   `config` ([`ConfigError`]).
//! * Nested parts encode as `{"variant","fields"}` objects ([`StoreError`],
//!   [`BackendError`]) or as their stable wire strings ([`NameError`],
//!   [`LifecycleState`], [`LifecycleCommand`], [`SourceLayer`]).
//! * An I/O source (`RegistryError::Io`) is carried as its message, and
//!   decodes to `std::io::Error::other(message)` — the same `Display`.
//!
//! The encoders match EXHAUSTIVELY (a new engine variant fails to compile
//! here until it is given a wire form). The decoder returns `None` for an
//! unknown schema version, family, variant or a missing field — the caller
//! then falls back to the envelope's own message.

use hekma_engine::ports::{BackendError, StoreError};
use hekma_engine::{
    ConfigError, EngineError, LifecycleCommand, LifecycleError, LifecycleState, NameError,
    RegistryError, SourceLayer,
};
use serde_json::{json, Map, Value};

/// The wire codec's own schema version (the `schema_version` field).
pub const WIRE_SCHEMA_VERSION: u32 = 1;

/// An engine error decoded from the wire — exactly one of the three engine
/// error families a Control API route can raise.
#[derive(Debug)]
pub enum WireError {
    /// An [`EngineError`] (lifecycle, interaction, drain).
    Engine(EngineError),
    /// A [`RegistryError`] (registration, removal, fleet reads, memory).
    Registry(RegistryError),
    /// A [`ConfigError`] (config reads and writes).
    Config(ConfigError),
}

/// Encode an [`EngineError`] into its wire object.
pub fn encode_engine(err: &EngineError) -> Value {
    let (variant, fields) = engine_parts(err);
    top("engine", variant, fields)
}

/// Encode a [`RegistryError`] into its wire object.
pub fn encode_registry(err: &RegistryError) -> Value {
    let (variant, fields) = registry_parts(err);
    top("registry", variant, fields)
}

/// Encode a [`ConfigError`] into its wire object.
pub fn encode_config(err: &ConfigError) -> Value {
    let (variant, fields) = config_parts(err);
    top("config", variant, fields)
}

/// Decode a wire object back into the engine's own error value. `None` when
/// the object is not a version this build understands, names an unknown
/// family or variant, or misses a field.
pub fn decode(value: &Value) -> Option<WireError> {
    let object = value.as_object()?;
    if object.get("schema_version")?.as_u64()? != u64::from(WIRE_SCHEMA_VERSION) {
        return None;
    }
    let variant = object.get("variant")?.as_str()?;
    let fields = object.get("fields")?.as_object()?;
    match object.get("family")?.as_str()? {
        "engine" => decode_engine(variant, fields).map(WireError::Engine),
        "registry" => decode_registry(variant, fields).map(WireError::Registry),
        "config" => decode_config(variant, fields).map(WireError::Config),
        _ => None,
    }
}

fn top(family: &str, variant: &str, fields: Value) -> Value {
    json!({
        "schema_version": WIRE_SCHEMA_VERSION,
        "family": family,
        "variant": variant,
        "fields": fields,
    })
}

fn nested(variant: &str, fields: Value) -> Value {
    json!({ "variant": variant, "fields": fields })
}

fn engine_parts(err: &EngineError) -> (&'static str, Value) {
    match err {
        EngineError::NotFound { name } => ("NotFound", json!({ "name": name })),
        EngineError::InvalidName { name, reason } => (
            "InvalidName",
            json!({ "name": name, "reason": name_error_wire(*reason) }),
        ),
        EngineError::InvalidTransition(LifecycleError::InvalidTransition { from, command }) => (
            "InvalidTransition",
            json!({ "from": from.as_str(), "command": command.as_str() }),
        ),
        EngineError::CapabilityUnsupported {
            name,
            capability,
            os,
            level,
        } => (
            "CapabilityUnsupported",
            json!({ "name": name, "capability": capability, "os": os, "level": level }),
        ),
        EngineError::ResumeUnsupported { name, os, level } => (
            "ResumeUnsupported",
            json!({ "name": name, "os": os, "level": level }),
        ),
        EngineError::LaunchFailed { name, detail } => {
            ("LaunchFailed", json!({ "name": name, "detail": detail }))
        }
        EngineError::AdapterUnresolved { name, detail } => (
            "AdapterUnresolved",
            json!({ "name": name, "detail": detail }),
        ),
        EngineError::Snapshot { name, path, detail } => (
            "Snapshot",
            json!({ "name": name, "path": path, "detail": detail }),
        ),
        EngineError::Secret { name, detail } => {
            ("Secret", json!({ "name": name, "detail": detail }))
        }
        EngineError::ObservedMetering { name, detail } => (
            "ObservedMetering",
            json!({ "name": name, "detail": detail }),
        ),
        EngineError::DetachRefused { name, detail } => {
            ("DetachRefused", json!({ "name": name, "detail": detail }))
        }
        EngineError::Log { name, path, detail } => (
            "Log",
            json!({ "name": name, "path": path, "detail": detail }),
        ),
        EngineError::Backend { name, source } => (
            "Backend",
            json!({ "name": name, "source": backend_wire(source) }),
        ),
        EngineError::NotRunning { name, state } => {
            ("NotRunning", json!({ "name": name, "state": state }))
        }
        EngineError::InteractionUnavailable { name, detail } => (
            "InteractionUnavailable",
            json!({ "name": name, "detail": detail }),
        ),
        EngineError::InteractionTimedOut { name, timeout_secs } => (
            "InteractionTimedOut",
            json!({ "name": name, "timeout_secs": timeout_secs }),
        ),
        EngineError::AcpTurnInFlight { name } => ("AcpTurnInFlight", json!({ "name": name })),
        EngineError::StopUnconfirmed { name, timeout_secs } => (
            "StopUnconfirmed",
            json!({ "name": name, "timeout_secs": timeout_secs }),
        ),
        EngineError::MemoryBackingChangedUnderStart {
            name,
            precheck,
            snapshot,
        } => (
            "MemoryBackingChangedUnderStart",
            json!({ "name": name, "precheck": precheck, "snapshot": snapshot }),
        ),
        EngineError::PauseCommandFailed {
            name,
            direction,
            command,
            detail,
        } => (
            "PauseCommandFailed",
            json!({ "name": name, "direction": direction, "command": command, "detail": detail }),
        ),
        EngineError::DrainRefused { name, state } => {
            ("DrainRefused", json!({ "name": name, "state": state }))
        }
        EngineError::DrainBoundInvalid { bound_ms } => {
            ("DrainBoundInvalid", json!({ "bound_ms": bound_ms }))
        }
        EngineError::Store(store) => ("Store", json!({ "store": store_wire(store) })),
    }
}

fn registry_parts(err: &RegistryError) -> (&'static str, Value) {
    match err {
        RegistryError::ServeOptionsOpenTimeOnly { detail } => {
            ("ServeOptionsOpenTimeOnly", json!({ "detail": detail }))
        }
        RegistryError::DuplicateName { name } => ("DuplicateName", json!({ "name": name })),
        RegistryError::InvalidName { name, reason } => (
            "InvalidName",
            json!({ "name": name, "reason": name_error_wire(*reason) }),
        ),
        RegistryError::NotFound { name } => ("NotFound", json!({ "name": name })),
        RegistryError::RunningRequiresForce { name } => {
            ("RunningRequiresForce", json!({ "name": name }))
        }
        RegistryError::MemoryBackingHotSwap { name, state } => (
            "MemoryBackingHotSwap",
            json!({ "name": name, "state": state }),
        ),
        RegistryError::MemoryBackingKindConflict {
            name,
            attached,
            requested,
        } => (
            "MemoryBackingKindConflict",
            json!({ "name": name, "attached": attached, "requested": requested }),
        ),
        RegistryError::MemoryBackingRowCommittedDirFailed { name, path, detail } => (
            "MemoryBackingRowCommittedDirFailed",
            json!({ "name": name, "path": path, "detail": detail }),
        ),
        RegistryError::OutboxOpenRefused { detail } => {
            ("OutboxOpenRefused", json!({ "detail": detail }))
        }
        RegistryError::Io { name, path, source } => (
            "Io",
            json!({ "name": name, "path": path, "source": source.to_string() }),
        ),
        RegistryError::RemoveLeftoverHome { name, path, detail } => (
            "RemoveLeftoverHome",
            json!({ "name": name, "path": path, "detail": detail }),
        ),
        RegistryError::RegisterOrphanRow {
            name,
            home_error,
            rollback_error,
        } => (
            "RegisterOrphanRow",
            json!({ "name": name, "home_error": home_error, "rollback_error": rollback_error }),
        ),
        RegistryError::SnapshotWrite { name, path, detail } => (
            "SnapshotWrite",
            json!({ "name": name, "path": path, "detail": detail }),
        ),
        RegistryError::UnknownAdapterKind { kind } => {
            ("UnknownAdapterKind", json!({ "kind": kind }))
        }
        RegistryError::ManifestNotFound { path } => ("ManifestNotFound", json!({ "path": path })),
        RegistryError::ManifestUnreadable { path, detail } => (
            "ManifestUnreadable",
            json!({ "path": path, "detail": detail }),
        ),
        RegistryError::ManifestInvalid { path, detail } => {
            ("ManifestInvalid", json!({ "path": path, "detail": detail }))
        }
        RegistryError::ContractIncompatible { path, detail } => (
            "ContractIncompatible",
            json!({ "path": path, "detail": detail }),
        ),
        RegistryError::NoMeteringSource { adapter } => {
            ("NoMeteringSource", json!({ "adapter": adapter }))
        }
        RegistryError::NoCapabilities { adapter } => {
            ("NoCapabilities", json!({ "adapter": adapter }))
        }
        RegistryError::Store(store) => ("Store", json!({ "store": store_wire(store) })),
    }
}

fn config_parts(err: &ConfigError) -> (&'static str, Value) {
    match err {
        ConfigError::UnknownKey { key, suggestion } => (
            "UnknownKey",
            json!({ "key": key, "suggestion": suggestion }),
        ),
        ConfigError::WriteShapeConflict {
            key,
            conflicting_ancestor,
        } => (
            "WriteShapeConflict",
            json!({ "key": key, "conflicting_ancestor": conflicting_ancestor }),
        ),
        ConfigError::InvalidValue { key, value, reason } => (
            "InvalidValue",
            json!({ "key": key, "value": value, "reason": reason }),
        ),
        ConfigError::MalformedLayer {
            layer,
            path,
            detail,
        } => (
            "MalformedLayer",
            json!({ "layer": layer.as_str(), "path": path, "detail": detail }),
        ),
        ConfigError::InvalidName { name, reason } => {
            ("InvalidName", json!({ "name": name, "reason": reason }))
        }
        ConfigError::NotFound { name } => ("NotFound", json!({ "name": name })),
        ConfigError::Store { name, detail } => ("Store", json!({ "name": name, "detail": detail })),
        ConfigError::SecretReveal { detail } => ("SecretReveal", json!({ "detail": detail })),
    }
}

fn store_wire(err: &StoreError) -> Value {
    match err {
        StoreError::DuplicateName { name } => nested("DuplicateName", json!({ "name": name })),
        StoreError::NotFound { name } => nested("NotFound", json!({ "name": name })),
        StoreError::CorruptRow { name, detail } => {
            nested("CorruptRow", json!({ "name": name, "detail": detail }))
        }
        StoreError::SchemaTooNew { found, supported } => nested(
            "SchemaTooNew",
            json!({ "found": found, "supported": supported }),
        ),
        StoreError::NeedsMigration { found, current } => nested(
            "NeedsMigration",
            json!({ "found": found, "current": current }),
        ),
        StoreError::MemoryBackingKindConflict {
            name,
            attached,
            requested,
        } => nested(
            "MemoryBackingKindConflict",
            json!({ "name": name, "attached": attached, "requested": requested }),
        ),
        StoreError::MemoryBackingHotSwap { name, state } => nested(
            "MemoryBackingHotSwap",
            json!({ "name": name, "state": state }),
        ),
        StoreError::MemoryBackingChangedUnderStart {
            name,
            precheck,
            snapshot,
        } => nested(
            "MemoryBackingChangedUnderStart",
            json!({ "name": name, "precheck": precheck, "snapshot": snapshot }),
        ),
        StoreError::StateConflict {
            name,
            expected,
            found,
        } => nested(
            "StateConflict",
            json!({ "name": name, "expected": expected, "found": found }),
        ),
        StoreError::Busy { waited_ms } => nested("Busy", json!({ "waited_ms": waited_ms })),
        StoreError::Backend(detail) => nested("Backend", json!({ "detail": detail })),
    }
}

fn backend_wire(err: &BackendError) -> Value {
    match err {
        BackendError::Spawn { exec, detail } => {
            nested("Spawn", json!({ "exec": exec, "detail": detail }))
        }
        BackendError::ImmediateExit { exec, code } => {
            nested("ImmediateExit", json!({ "exec": exec, "code": code }))
        }
        BackendError::Control { op, detail } => {
            nested("Control", json!({ "op": op, "detail": detail }))
        }
        BackendError::StdinTimedOut { timeout_secs } => {
            nested("StdinTimedOut", json!({ "timeout_secs": timeout_secs }))
        }
        BackendError::StopUnconfirmed { timeout_secs } => {
            nested("StopUnconfirmed", json!({ "timeout_secs": timeout_secs }))
        }
    }
}

fn name_error_wire(reason: NameError) -> &'static str {
    match reason {
        NameError::Empty => "empty",
        NameError::BadFirstChar => "bad_first_char",
        NameError::BadChar => "bad_char",
        NameError::TooLong => "too_long",
    }
}

type Fields = Map<String, Value>;

fn text(fields: &Fields, key: &str) -> Option<String> {
    fields.get(key)?.as_str().map(str::to_string)
}

fn unsigned(fields: &Fields, key: &str) -> Option<u64> {
    fields.get(key)?.as_u64()
}

fn signed(fields: &Fields, key: &str) -> Option<i64> {
    fields.get(key)?.as_i64()
}

fn name_error(fields: &Fields, key: &str) -> Option<NameError> {
    match fields.get(key)?.as_str()? {
        "empty" => Some(NameError::Empty),
        "bad_first_char" => Some(NameError::BadFirstChar),
        "bad_char" => Some(NameError::BadChar),
        "too_long" => Some(NameError::TooLong),
        _ => None,
    }
}

fn lifecycle_command(value: &str) -> Option<LifecycleCommand> {
    [
        LifecycleCommand::Start,
        LifecycleCommand::Stop,
        LifecycleCommand::Pause,
        LifecycleCommand::Resume,
    ]
    .into_iter()
    .find(|command| command.as_str() == value)
}

fn source_layer(value: &str) -> Option<SourceLayer> {
    SourceLayer::ORDER
        .into_iter()
        .find(|layer| layer.as_str() == value)
}

/// The `BackendError::Control` op is a `&'static str` naming the control
/// operation. The known ops map to their static strings; an op this build
/// does not know (a newer host) is leaked once — the decoder runs in a
/// short-lived CLI process on an error path, so the bounded leak buys an
/// exact rendering.
fn static_op(op: String) -> &'static str {
    const KNOWN: [&str; 8] = [
        "poll",
        "resume",
        "signal",
        "stdin",
        "stop",
        "terminate",
        "wait",
        "pause",
    ];
    match KNOWN.iter().find(|known| **known == op) {
        Some(known) => known,
        None => Box::leak(op.into_boxed_str()),
    }
}

fn nested_parts(value: &Value) -> Option<(&str, &Fields)> {
    let object = value.as_object()?;
    Some((
        object.get("variant")?.as_str()?,
        object.get("fields")?.as_object()?,
    ))
}

fn decode_store(value: &Value) -> Option<StoreError> {
    let (variant, f) = nested_parts(value)?;
    Some(match variant {
        "DuplicateName" => StoreError::DuplicateName {
            name: text(f, "name")?,
        },
        "NotFound" => StoreError::NotFound {
            name: text(f, "name")?,
        },
        "CorruptRow" => StoreError::CorruptRow {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "SchemaTooNew" => StoreError::SchemaTooNew {
            found: signed(f, "found")?,
            supported: signed(f, "supported")?,
        },
        "NeedsMigration" => StoreError::NeedsMigration {
            found: signed(f, "found")?,
            current: signed(f, "current")?,
        },
        "MemoryBackingKindConflict" => StoreError::MemoryBackingKindConflict {
            name: text(f, "name")?,
            attached: text(f, "attached")?,
            requested: text(f, "requested")?,
        },
        "MemoryBackingHotSwap" => StoreError::MemoryBackingHotSwap {
            name: text(f, "name")?,
            state: text(f, "state")?,
        },
        "MemoryBackingChangedUnderStart" => StoreError::MemoryBackingChangedUnderStart {
            name: text(f, "name")?,
            precheck: text(f, "precheck")?,
            snapshot: text(f, "snapshot")?,
        },
        "StateConflict" => StoreError::StateConflict {
            name: text(f, "name")?,
            expected: text(f, "expected")?,
            found: text(f, "found")?,
        },
        "Busy" => StoreError::Busy {
            waited_ms: unsigned(f, "waited_ms")?,
        },
        "Backend" => StoreError::Backend(text(f, "detail")?),
        _ => return None,
    })
}

fn decode_backend(value: &Value) -> Option<BackendError> {
    let (variant, f) = nested_parts(value)?;
    Some(match variant {
        "Spawn" => BackendError::Spawn {
            exec: text(f, "exec")?,
            detail: text(f, "detail")?,
        },
        "ImmediateExit" => BackendError::ImmediateExit {
            exec: text(f, "exec")?,
            code: i32::try_from(signed(f, "code")?).ok()?,
        },
        "Control" => BackendError::Control {
            op: static_op(text(f, "op")?),
            detail: text(f, "detail")?,
        },
        "StdinTimedOut" => BackendError::StdinTimedOut {
            timeout_secs: unsigned(f, "timeout_secs")?,
        },
        "StopUnconfirmed" => BackendError::StopUnconfirmed {
            timeout_secs: unsigned(f, "timeout_secs")?,
        },
        _ => return None,
    })
}

fn decode_engine(variant: &str, f: &Fields) -> Option<EngineError> {
    Some(match variant {
        "NotFound" => EngineError::NotFound {
            name: text(f, "name")?,
        },
        "InvalidName" => EngineError::InvalidName {
            name: text(f, "name")?,
            reason: name_error(f, "reason")?,
        },
        "InvalidTransition" => EngineError::InvalidTransition(LifecycleError::InvalidTransition {
            from: LifecycleState::from_wire(&text(f, "from")?)?,
            command: lifecycle_command(&text(f, "command")?)?,
        }),
        "CapabilityUnsupported" => EngineError::CapabilityUnsupported {
            name: text(f, "name")?,
            capability: text(f, "capability")?,
            os: text(f, "os")?,
            level: text(f, "level")?,
        },
        "ResumeUnsupported" => EngineError::ResumeUnsupported {
            name: text(f, "name")?,
            os: text(f, "os")?,
            level: text(f, "level")?,
        },
        "LaunchFailed" => EngineError::LaunchFailed {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "AdapterUnresolved" => EngineError::AdapterUnresolved {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "Snapshot" => EngineError::Snapshot {
            name: text(f, "name")?,
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "Secret" => EngineError::Secret {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "ObservedMetering" => EngineError::ObservedMetering {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "DetachRefused" => EngineError::DetachRefused {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "Log" => EngineError::Log {
            name: text(f, "name")?,
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "Backend" => EngineError::Backend {
            name: text(f, "name")?,
            source: decode_backend(f.get("source")?)?,
        },
        "NotRunning" => EngineError::NotRunning {
            name: text(f, "name")?,
            state: text(f, "state")?,
        },
        "InteractionUnavailable" => EngineError::InteractionUnavailable {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "InteractionTimedOut" => EngineError::InteractionTimedOut {
            name: text(f, "name")?,
            timeout_secs: unsigned(f, "timeout_secs")?,
        },
        "AcpTurnInFlight" => EngineError::AcpTurnInFlight {
            name: text(f, "name")?,
        },
        "StopUnconfirmed" => EngineError::StopUnconfirmed {
            name: text(f, "name")?,
            timeout_secs: unsigned(f, "timeout_secs")?,
        },
        "MemoryBackingChangedUnderStart" => EngineError::MemoryBackingChangedUnderStart {
            name: text(f, "name")?,
            precheck: text(f, "precheck")?,
            snapshot: text(f, "snapshot")?,
        },
        "PauseCommandFailed" => EngineError::PauseCommandFailed {
            name: text(f, "name")?,
            direction: text(f, "direction")?,
            command: text(f, "command")?,
            detail: text(f, "detail")?,
        },
        "DrainRefused" => EngineError::DrainRefused {
            name: text(f, "name")?,
            state: text(f, "state")?,
        },
        "DrainBoundInvalid" => EngineError::DrainBoundInvalid {
            bound_ms: unsigned(f, "bound_ms")?,
        },
        "Store" => EngineError::Store(decode_store(f.get("store")?)?),
        _ => return None,
    })
}

fn decode_registry(variant: &str, f: &Fields) -> Option<RegistryError> {
    Some(match variant {
        "ServeOptionsOpenTimeOnly" => RegistryError::ServeOptionsOpenTimeOnly {
            detail: text(f, "detail")?,
        },
        "DuplicateName" => RegistryError::DuplicateName {
            name: text(f, "name")?,
        },
        "InvalidName" => RegistryError::InvalidName {
            name: text(f, "name")?,
            reason: name_error(f, "reason")?,
        },
        "NotFound" => RegistryError::NotFound {
            name: text(f, "name")?,
        },
        "RunningRequiresForce" => RegistryError::RunningRequiresForce {
            name: text(f, "name")?,
        },
        "MemoryBackingHotSwap" => RegistryError::MemoryBackingHotSwap {
            name: text(f, "name")?,
            state: text(f, "state")?,
        },
        "MemoryBackingKindConflict" => RegistryError::MemoryBackingKindConflict {
            name: text(f, "name")?,
            attached: text(f, "attached")?,
            requested: text(f, "requested")?,
        },
        "MemoryBackingRowCommittedDirFailed" => RegistryError::MemoryBackingRowCommittedDirFailed {
            name: text(f, "name")?,
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "OutboxOpenRefused" => RegistryError::OutboxOpenRefused {
            detail: text(f, "detail")?,
        },
        "Io" => RegistryError::Io {
            name: text(f, "name")?,
            path: text(f, "path")?,
            source: std::io::Error::other(text(f, "source")?),
        },
        "RemoveLeftoverHome" => RegistryError::RemoveLeftoverHome {
            name: text(f, "name")?,
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "RegisterOrphanRow" => RegistryError::RegisterOrphanRow {
            name: text(f, "name")?,
            home_error: text(f, "home_error")?,
            rollback_error: text(f, "rollback_error")?,
        },
        "SnapshotWrite" => RegistryError::SnapshotWrite {
            name: text(f, "name")?,
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "UnknownAdapterKind" => RegistryError::UnknownAdapterKind {
            kind: text(f, "kind")?,
        },
        "ManifestNotFound" => RegistryError::ManifestNotFound {
            path: text(f, "path")?,
        },
        "ManifestUnreadable" => RegistryError::ManifestUnreadable {
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "ManifestInvalid" => RegistryError::ManifestInvalid {
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "ContractIncompatible" => RegistryError::ContractIncompatible {
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "NoMeteringSource" => RegistryError::NoMeteringSource {
            adapter: text(f, "adapter")?,
        },
        "NoCapabilities" => RegistryError::NoCapabilities {
            adapter: text(f, "adapter")?,
        },
        "Store" => RegistryError::Store(decode_store(f.get("store")?)?),
        _ => return None,
    })
}

fn decode_config(variant: &str, f: &Fields) -> Option<ConfigError> {
    Some(match variant {
        "UnknownKey" => ConfigError::UnknownKey {
            key: text(f, "key")?,
            // `null` is the honest "no close match" — present, not missing.
            suggestion: match f.get("suggestion")? {
                Value::Null => None,
                Value::String(nearest) => Some(nearest.clone()),
                _ => return None,
            },
        },
        "WriteShapeConflict" => ConfigError::WriteShapeConflict {
            key: text(f, "key")?,
            conflicting_ancestor: text(f, "conflicting_ancestor")?,
        },
        "InvalidValue" => ConfigError::InvalidValue {
            key: text(f, "key")?,
            value: text(f, "value")?,
            reason: text(f, "reason")?,
        },
        "MalformedLayer" => ConfigError::MalformedLayer {
            layer: source_layer(&text(f, "layer")?)?,
            path: text(f, "path")?,
            detail: text(f, "detail")?,
        },
        "InvalidName" => ConfigError::InvalidName {
            name: text(f, "name")?,
            reason: text(f, "reason")?,
        },
        "NotFound" => ConfigError::NotFound {
            name: text(f, "name")?,
        },
        "Store" => ConfigError::Store {
            name: text(f, "name")?,
            detail: text(f, "detail")?,
        },
        "SecretReveal" => ConfigError::SecretReveal {
            detail: text(f, "detail")?,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
