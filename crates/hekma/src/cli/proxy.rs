//! `hekma proxy` (story 21-8): the shared Metering Proxy's Operator
//! settings. `show` reads the state directory read-only and starts no
//! Service Host (a running host is asked, never started, whether a serve
//! flag overrides the stored setting); `set` is one `PATCH /v0/proxy`
//! routed to the state root's local Service Host, which stores the setting
//! and publishes it before it answers, so the very next forwarded call
//! follows it.

use std::time::Duration;

use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route, state_root, RoutedError};
use super::host_client::{CallError, HostClient};
use crate::cli::run::map_run_code;

/// The `--json` document's schema version (the proxy family).
const PROXY_DOCUMENT_SCHEMA_VERSION: u32 = 1;

fn word(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// `hekma proxy show [--json]`.
pub fn show(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let stored = engine
        .blocking()
        .proxy_settings()
        .map_err(|err| map_run_code(err.code(), err.to_string()))?
        .stored_identity_headers;
    let port = engine.recorded_metering_port();
    drop(engine);
    // A running host knows its serve flag; ask it (bounded), never start one.
    let live = state_root()
        .ok()
        .and_then(|root| HostClient::find(&root))
        .and_then(|host| {
            host.bounded(Duration::from_secs(5))
                .call("GET", "/v0/proxy", None)
                .ok()
                .flatten()
        });
    let forced = live
        .as_ref()
        .and_then(|doc| doc["forced_by_flag"].as_bool())
        .unwrap_or(false);
    let effective = live
        .as_ref()
        .and_then(|doc| doc["identity_headers"].as_bool())
        .unwrap_or(stored);
    let url = port.map(|port| format!("http://127.0.0.1:{port}"));
    if json {
        let document = json!({
            "schema_version": PROXY_DOCUMENT_SCHEMA_VERSION,
            "url": url,
            "port": port,
            "identity_headers": effective,
            "stored_identity_headers": stored,
            "forced_by_flag": forced,
        });
        println!("{}", serde_json::to_string_pretty(&document).unwrap());
        return Ok(());
    }
    match &url {
        Some(url) => println!("metering proxy {url}"),
        None => println!(
            "metering proxy: no address recorded yet (the first `hekma serve` records one)"
        ),
    }
    if forced {
        println!(
            "identity-headers {} (forced by a serve flag; the stored setting is {})",
            word(effective),
            word(stored)
        );
    } else {
        println!("identity-headers {}", word(effective));
    }
    Ok(())
}

/// `hekma proxy set identity-headers <on|off> [--json]`.
pub fn set(on: bool, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let routed = route(
        "PATCH",
        "/v0/proxy",
        Some(json!({ "identity_headers": on })),
    )
    .map_err(|err| match err {
        RoutedError::Call(CallError::Coded { code, message }) => map_run_code(&code, message),
        other => other.into_diagnostic(),
    })?;
    let document = routed.value.unwrap_or(Value::Null);
    if json {
        let document = json!({
            "schema_version": PROXY_DOCUMENT_SCHEMA_VERSION,
            "identity_headers": document["identity_headers"],
            "stored_identity_headers": document["stored_identity_headers"],
            "forced_by_flag": document["forced_by_flag"],
        });
        println!("{}", serde_json::to_string_pretty(&document).unwrap());
        return Ok(());
    }
    println!(
        "identity-headers {}: applies to the next forwarded call",
        word(on)
    );
    if document["forced_by_flag"].as_bool() == Some(true) {
        crate::ui::note(format!(
            "this host run was started with a serve flag that keeps identity headers {}; the \
             stored setting applies from the next start without it",
            word(document["identity_headers"].as_bool().unwrap_or(on))
        ));
    }
    Ok(())
}
