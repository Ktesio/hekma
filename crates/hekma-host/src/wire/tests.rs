//! The wire codec's round trip, per variant (story 21-4): every engine error a
//! routed write can raise encodes, crosses JSON text, and decodes back to a
//! value whose `Debug` (and so its `Display`) is identical — the CLI then
//! renders the same diagnostic and exit code it rendered in-process.

use super::*;

fn store_variants() -> Vec<StoreError> {
    vec![
        StoreError::DuplicateName { name: "a".into() },
        StoreError::NotFound { name: "a".into() },
        StoreError::CorruptRow {
            name: "a".into(),
            detail: "bad".into(),
        },
        StoreError::SchemaTooNew {
            found: 99,
            supported: 12,
        },
        StoreError::NeedsMigration {
            found: 3,
            current: 12,
        },
        StoreError::MemoryBackingKindConflict {
            name: "a".into(),
            attached: "native".into(),
            requested: "filesystem".into(),
        },
        StoreError::MemoryBackingHotSwap {
            name: "a".into(),
            state: "running".into(),
        },
        StoreError::MemoryBackingChangedUnderStart {
            name: "a".into(),
            precheck: "none".into(),
            snapshot: "filesystem".into(),
        },
        StoreError::StateConflict {
            name: "a".into(),
            expected: "stopped".into(),
            found: "starting".into(),
        },
        StoreError::Busy { waited_ms: 1000 },
        StoreError::Backend("disk I/O error".into()),
    ]
}

fn backend_variants() -> Vec<BackendError> {
    vec![
        BackendError::Spawn {
            exec: "/bin/agent".into(),
            detail: "no such file".into(),
        },
        BackendError::ImmediateExit {
            exec: "/bin/agent".into(),
            code: -3,
        },
        BackendError::Control {
            op: "terminate",
            detail: "EPERM".into(),
        },
        BackendError::Control {
            op: "some-future-op",
            detail: "x".into(),
        },
        BackendError::StdinTimedOut { timeout_secs: 5 },
        BackendError::StopUnconfirmed { timeout_secs: 7 },
    ]
}

fn engine_variants() -> Vec<EngineError> {
    let mut all = vec![
        EngineError::NotFound { name: "a".into() },
        EngineError::InvalidTransition(LifecycleError::InvalidTransition {
            from: LifecycleState::Stopped,
            command: LifecycleCommand::Stop,
        }),
        EngineError::CapabilityUnsupported {
            name: "a".into(),
            capability: "pause".into(),
            os: "linux".into(),
            level: "unsupported".into(),
        },
        EngineError::ResumeUnsupported {
            name: "a".into(),
            os: "linux".into(),
            level: "unsupported".into(),
        },
        EngineError::LaunchFailed {
            name: "a".into(),
            detail: "boom".into(),
        },
        EngineError::AdapterUnresolved {
            name: "a".into(),
            detail: "gone".into(),
        },
        EngineError::Snapshot {
            name: "a".into(),
            path: "/s".into(),
            detail: "denied".into(),
        },
        EngineError::Secret {
            name: "a".into(),
            detail: "unset".into(),
        },
        EngineError::ObservedMetering {
            name: "a".into(),
            detail: "bind".into(),
        },
        EngineError::Custody {
            name: "a".into(),
            detail: "second delivery".into(),
        },
        EngineError::DetachRefused {
            name: "a".into(),
            detail: "observed".into(),
        },
        EngineError::Log {
            name: "a".into(),
            path: "/l".into(),
            detail: "full".into(),
        },
        EngineError::NotRunning {
            name: "a".into(),
            state: "paused".into(),
        },
        EngineError::InteractionUnavailable {
            name: "a".into(),
            detail: "no pipe".into(),
        },
        EngineError::InteractionTimedOut {
            name: "a".into(),
            timeout_secs: 5,
        },
        EngineError::AcpTurnInFlight { name: "a".into() },
        EngineError::StopUnconfirmed {
            name: "a".into(),
            timeout_secs: 9,
        },
        EngineError::MemoryBackingChangedUnderStart {
            name: "a".into(),
            precheck: "none".into(),
            snapshot: "native".into(),
        },
        EngineError::PauseCommandFailed {
            name: "a".into(),
            direction: "pause".into(),
            command: "x --y".into(),
            detail: "exit 3".into(),
        },
        EngineError::DrainRefused {
            name: "a".into(),
            state: "stopped".into(),
        },
        EngineError::DrainBoundInvalid { bound_ms: 0 },
    ];
    for reason in [
        NameError::Empty,
        NameError::BadFirstChar,
        NameError::BadChar,
        NameError::TooLong,
    ] {
        all.push(EngineError::InvalidName {
            name: "Bad".into(),
            reason,
        });
    }
    for state in [
        LifecycleState::Registered,
        LifecycleState::Starting,
        LifecycleState::Running,
        LifecycleState::Paused,
        LifecycleState::Stopping,
        LifecycleState::Failed,
    ] {
        for command in [
            LifecycleCommand::Start,
            LifecycleCommand::Pause,
            LifecycleCommand::Resume,
        ] {
            all.push(EngineError::InvalidTransition(
                LifecycleError::InvalidTransition {
                    from: state,
                    command,
                },
            ));
        }
    }
    all.extend(
        backend_variants()
            .into_iter()
            .map(|source| EngineError::Backend {
                name: "a".into(),
                source,
            }),
    );
    all.extend(store_variants().into_iter().map(EngineError::Store));
    all
}

fn registry_variants() -> Vec<RegistryError> {
    let mut all = vec![
        RegistryError::ServeOptionsOpenTimeOnly {
            detail: "twice".into(),
        },
        RegistryError::DuplicateName { name: "a".into() },
        RegistryError::InvalidName {
            name: "Bad".into(),
            reason: NameError::BadFirstChar,
        },
        RegistryError::NotFound { name: "a".into() },
        RegistryError::RunningRequiresForce { name: "a".into() },
        RegistryError::MemoryBackingHotSwap {
            name: "a".into(),
            state: "running".into(),
        },
        RegistryError::MemoryBackingKindConflict {
            name: "a".into(),
            attached: "native".into(),
            requested: "filesystem".into(),
        },
        RegistryError::MemoryBackingRowCommittedDirFailed {
            name: "a".into(),
            path: "/m".into(),
            detail: "denied".into(),
        },
        RegistryError::OutboxOpenRefused {
            detail: "torn".into(),
        },
        RegistryError::Io {
            name: "a".into(),
            path: "/p".into(),
            source: std::io::Error::other("Permission denied (os error 13)"),
        },
        RegistryError::RemoveLeftoverHome {
            name: "a".into(),
            path: "/h".into(),
            detail: "busy".into(),
        },
        RegistryError::RegisterOrphanRow {
            name: "a".into(),
            home_error: "h".into(),
            rollback_error: "r".into(),
        },
        RegistryError::SnapshotWrite {
            name: "a".into(),
            path: "/s".into(),
            detail: "d".into(),
        },
        RegistryError::UnknownAdapterKind {
            kind: "nope".into(),
        },
        RegistryError::ManifestNotFound { path: "/m".into() },
        RegistryError::ManifestUnreadable {
            path: "/m".into(),
            detail: "d".into(),
        },
        RegistryError::ManifestInvalid {
            path: "/m".into(),
            detail: "d".into(),
        },
        RegistryError::ContractIncompatible {
            path: "/m".into(),
            detail: "v2".into(),
        },
        RegistryError::NoMeteringSource {
            adapter: "x".into(),
        },
        RegistryError::NoCapabilities {
            adapter: "x".into(),
        },
    ];
    all.extend(store_variants().into_iter().map(RegistryError::Store));
    all
}

fn config_variants() -> Vec<ConfigError> {
    let mut all = vec![
        ConfigError::UnknownKey {
            key: "modle".into(),
            suggestion: Some("model".into()),
        },
        ConfigError::UnknownKey {
            key: "zzz".into(),
            suggestion: None,
        },
        ConfigError::WriteShapeConflict {
            key: "a.b".into(),
            conflicting_ancestor: "a".into(),
        },
        ConfigError::InvalidValue {
            key: "k".into(),
            value: "v".into(),
            reason: "r".into(),
        },
        ConfigError::InvalidName {
            name: "Bad".into(),
            reason: "rule".into(),
        },
        ConfigError::NotFound { name: "a".into() },
        ConfigError::Store {
            name: "a".into(),
            detail: "locked".into(),
        },
        ConfigError::SecretReveal {
            detail: "unset".into(),
        },
    ];
    for layer in SourceLayer::ORDER {
        all.push(ConfigError::MalformedLayer {
            layer,
            path: "/c".into(),
            detail: "toml".into(),
        });
    }
    all
}

/// Encode, cross JSON TEXT (the real wire), decode.
fn through_the_wire(encoded: Value) -> Option<WireError> {
    let text = serde_json::to_string(&encoded).expect("serialize");
    let parsed: Value = serde_json::from_str(&text).expect("parse");
    decode(&parsed)
}

#[test]
fn every_engine_error_variant_round_trips() {
    for err in engine_variants() {
        let before = format!("{err:?} / {err}");
        match through_the_wire(encode_engine(&err)) {
            Some(WireError::Engine(back)) => assert_eq!(format!("{back:?} / {back}"), before),
            other => panic!("{before} decoded as {other:?}"),
        }
    }
}

#[test]
fn every_registry_error_variant_round_trips() {
    for err in registry_variants() {
        let before = format!("{err:?} / {err}");
        match through_the_wire(encode_registry(&err)) {
            Some(WireError::Registry(back)) => assert_eq!(format!("{back:?} / {back}"), before),
            other => panic!("{before} decoded as {other:?}"),
        }
    }
}

#[test]
fn every_config_error_variant_round_trips() {
    for err in config_variants() {
        let before = format!("{err:?} / {err}");
        match through_the_wire(encode_config(&err)) {
            Some(WireError::Config(back)) => assert_eq!(format!("{back:?} / {back}"), before),
            other => panic!("{before} decoded as {other:?}"),
        }
    }
}

#[test]
fn an_io_source_crosses_as_its_message() {
    // A real OS error's Display (with its code) is what the CLI prints.
    let source = std::io::Error::from_raw_os_error(2);
    let shown = source.to_string();
    let err = RegistryError::Io {
        name: "a".into(),
        path: "/p".into(),
        source,
    };
    let Some(WireError::Registry(back)) = through_the_wire(encode_registry(&err)) else {
        panic!("decode");
    };
    assert_eq!(back.to_string(), err.to_string());
    assert!(back.to_string().contains(&shown));
}

#[test]
fn the_object_names_its_family_variant_and_version() {
    let encoded = encode_engine(&EngineError::NotFound { name: "w".into() });
    assert_eq!(encoded["schema_version"], 1);
    assert_eq!(encoded["family"], "engine");
    assert_eq!(encoded["variant"], "NotFound");
    assert_eq!(encoded["fields"]["name"], "w");
    let nested = encode_registry(&RegistryError::Store(StoreError::Busy { waited_ms: 5 }));
    assert_eq!(nested["fields"]["store"]["variant"], "Busy");
}

#[test]
fn unknown_shapes_decode_to_none() {
    let good = encode_engine(&EngineError::NotFound { name: "w".into() });
    let mut wrong_version = good.clone();
    wrong_version["schema_version"] = json!(2);
    assert!(decode(&wrong_version).is_none());
    let mut wrong_family = good.clone();
    wrong_family["family"] = json!("proxy");
    assert!(decode(&wrong_family).is_none());
    let mut wrong_variant = good.clone();
    wrong_variant["variant"] = json!("FromTheFuture");
    assert!(decode(&wrong_variant).is_none());
    let mut missing_field = good.clone();
    missing_field["fields"] = json!({});
    assert!(decode(&missing_field).is_none());
    assert!(decode(&json!("text")).is_none());
    assert!(decode(&json!({"schema_version": 1})).is_none());
    // Unknown nested variants and wire strings are refused too.
    for broken in [
        json!({"schema_version":1,"family":"engine","variant":"Store","fields":{"store":{"variant":"New","fields":{}}}}),
        json!({"schema_version":1,"family":"engine","variant":"Backend","fields":{"name":"a","source":{"variant":"New","fields":{}}}}),
        json!({"schema_version":1,"family":"engine","variant":"InvalidName","fields":{"name":"a","reason":"odd"}}),
        json!({"schema_version":1,"family":"engine","variant":"InvalidTransition","fields":{"from":"running","command":"jump"}}),
        json!({"schema_version":1,"family":"config","variant":"MalformedLayer","fields":{"layer":"cloud","path":"/","detail":"d"}}),
        json!({"schema_version":1,"family":"config","variant":"UnknownKey","fields":{"key":"k","suggestion":3}}),
        json!({"schema_version":1,"family":"registry","variant":"Nope","fields":{}}),
        json!({"schema_version":1,"family":"config","variant":"Nope","fields":{}}),
    ] {
        assert!(decode(&broken).is_none(), "{broken}");
    }
}
