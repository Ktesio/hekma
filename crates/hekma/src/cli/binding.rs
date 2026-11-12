//! `hekma binding` — endpoint bindings (the shared Metering Proxy's
//! Operator surface).
//!
//! A binding says: calls that arrive on this path prefix forward to this
//! upstream, and the Runs they are attributed to carry these labels. Reads
//! open `state.db` read-only and start no Service Host; each write is one
//! `/v0/bindings` call routed to the state root's local Service Host.
//!
//! `show` prints the CLIENT BASE URL — the proxy's recorded address with
//! the binding's prefix appended, the URL a client points at.

use hekma_engine::{BindingCredentialClass, BindingError};
use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route, RoutedError};
use super::host_client::CallError;
use crate::cli::run::map_run_code;
use crate::error::RunInvalid;

/// The `--json` document's schema version (the binding family).
const BINDING_DOCUMENT_SCHEMA_VERSION: u32 = 1;

fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error> {
    RunInvalid {
        message: message.into(),
    }
    .into()
}

/// A routed refusal as the CLI's exit class.
fn routed_error(err: RoutedError) -> Box<dyn std::error::Error> {
    match err {
        RoutedError::Call(CallError::Coded { code, message }) => map_run_code(&code, message),
        other => other.into_diagnostic(),
    }
}

fn engine_error(err: BindingError) -> Box<dyn std::error::Error> {
    let message = err.to_string();
    match err {
        BindingError::NotFound { .. } | BindingError::RouteNotFound { .. } => {
            crate::error::RunNotFound { message }.into()
        }
        other => RunInvalid {
            message: other.to_string(),
        }
        .into(),
    }
}

/// `--label k=v` values.
fn parse_label(flag: &str, raw: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| invalid(format!("--{flag}: `{raw}` is not key=value")))?;
    if key.is_empty() || value.is_empty() {
        return Err(invalid(format!("--{flag}: `{raw}` is not key=value")));
    }
    Ok((key.to_string(), value.to_string()))
}

/// `hekma binding create <name> --prefix <p> --upstream <url> [--label k=v]...
/// [--custodied-key secret:NAME]`.
pub struct CreateArgs {
    pub name: String,
    pub prefix: String,
    pub upstream: String,
    pub labels: Vec<String>,
    pub custodied_key: Option<String>,
    pub json: bool,
}

/// `hekma binding set <name> [--prefix] [--upstream] [--label]...
/// [--clear-labels] [--custodied-key|--clear-custodied-key]`.
pub struct SetArgs {
    pub name: String,
    pub prefix: Option<String>,
    pub upstream: Option<String>,
    pub labels: Vec<String>,
    pub clear_labels: bool,
    pub custodied_key: Option<String>,
    pub clear_custodied_key: bool,
    pub json: bool,
}

/// `hekma binding route add <name> <route> --upstream <url>`.
pub struct RouteArgs {
    pub name: String,
    pub route: String,
    pub upstream: String,
    pub custodied_key: Option<String>,
    pub json: bool,
}

pub fn create(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut labels = Vec::new();
    for raw in &args.labels {
        labels.push(parse_label("label", raw)?);
    }
    let body = json!({
        "name": args.name,
        "prefix": args.prefix,
        "upstream": args.upstream,
        "labels": labels,
        "custodied_key": args.custodied_key,
    });
    let routed = route("POST", "/v0/bindings", Some(body)).map_err(routed_error)?;
    let binding = routed.value.unwrap_or(Value::Null)["binding"].clone();
    if args.json {
        print_document("binding", &binding);
    } else {
        print_summary(&binding, None);
        println!("created");
    }
    Ok(())
}

pub fn list(
    after: Option<String>,
    limit: usize,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let bindings = engine
        .blocking()
        .bindings(after.as_deref(), limit)
        .map_err(engine_error)?;
    if json {
        let document = json!({
            "schema_version": BINDING_DOCUMENT_SCHEMA_VERSION,
            "bindings": bindings,
        });
        println!("{}", serde_json::to_string_pretty(&document).unwrap());
        return Ok(());
    }
    if bindings.is_empty() {
        println!("no bindings");
        return Ok(());
    }
    for binding in &bindings {
        print_summary(&serde_json::to_value(binding).unwrap(), None);
    }
    Ok(())
}

pub fn show(name: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let binding = engine
        .blocking()
        .binding(name)
        .map_err(engine_error)?
        .ok_or_else(|| crate::error::RunNotFound {
            message: format!("no binding is named `{name}`"),
        })?;
    // The client base URL: the recorded proxy address + the prefix. Without
    // a recorded port (no host ever served this directory) the honest line
    // names the remediation instead of guessing.
    let port = engine.blocking().recorded_metering_port();
    let base = port.map(|port| format!("http://127.0.0.1:{port}{}", binding.prefix));
    let value = serde_json::to_value(&binding).unwrap();
    if json {
        let document = json!({
            "schema_version": BINDING_DOCUMENT_SCHEMA_VERSION,
            "binding": value,
            "client_base_url": base,
        });
        println!("{}", serde_json::to_string_pretty(&document).unwrap());
        return Ok(());
    }
    print_summary(&value, base.as_deref());
    for route in &binding.routes {
        println!(
            "  route {} -> {}{}",
            route.name,
            route.upstream,
            route
                .custodied_key
                .as_ref()
                .map(|key| format!(" (custodied key {key})"))
                .unwrap_or_default()
        );
    }
    if base.is_none() {
        println!(
            "  no metering proxy port is recorded for this state directory yet — start the \
             Service Host (`hekma serve`) and the client base URL appears here"
        );
    }
    Ok(())
}

pub fn set(args: SetArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.clear_labels
        && args.prefix.is_none()
        && args.upstream.is_none()
        && args.labels.is_empty()
        && args.custodied_key.is_none()
        && !args.clear_custodied_key
    {
        return Err(invalid(
            "nothing to set: pass --prefix, --upstream, --label, --clear-labels or \
             --custodied-key",
        ));
    }
    let mut patch = json!({});
    if let Some(prefix) = args.prefix {
        patch["prefix"] = json!(prefix);
    }
    if let Some(upstream) = args.upstream {
        patch["upstream"] = json!(upstream);
    }
    if !args.labels.is_empty() {
        let mut labels = Vec::new();
        for raw in &args.labels {
            labels.push(parse_label("label", raw)?);
        }
        patch["labels"] = json!(labels);
    }
    if args.clear_labels {
        patch["labels"] = json!([]);
    }
    if let Some(key) = args.custodied_key {
        patch["custodied_key"] = json!(key);
    }
    if args.clear_custodied_key {
        patch["custodied_key"] = Value::Null;
    }
    let path = format!("/v0/bindings/{}", args.name);
    let routed = route("PATCH", &path, Some(patch)).map_err(routed_error)?;
    let binding = routed.value.unwrap_or(Value::Null)["binding"].clone();
    if args.json {
        print_document("binding", &binding);
    } else {
        print_summary(&binding, None);
        println!("updated");
    }
    Ok(())
}

pub fn delete(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = format!("/v0/bindings/{name}");
    route("DELETE", &path, None).map_err(routed_error)?;
    println!("{name} deleted (its Run ended with cause binding-removed)");
    Ok(())
}

pub fn route_add(args: RouteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let body = json!({
        "name": args.route,
        "upstream": args.upstream,
        "custodied_key": args.custodied_key,
    });
    let path = format!("/v0/bindings/{}/routes/{}", args.name, args.route);
    let routed = route("PUT", &path, Some(body)).map_err(routed_error)?;
    let binding = routed.value.unwrap_or(Value::Null)["binding"].clone();
    if args.json {
        print_document("binding", &binding);
    } else {
        print_summary(&binding, None);
        println!("route {} set (used when reroute ships)", args.route);
    }
    Ok(())
}

pub fn route_remove(
    name: &str,
    route_name: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = format!("/v0/bindings/{name}/routes/{route_name}");
    let routed = route("DELETE", &path, None).map_err(routed_error)?;
    let binding = routed.value.unwrap_or(Value::Null)["binding"].clone();
    if json {
        print_document("binding", &binding);
    } else {
        print_summary(&binding, None);
        println!("route {route_name} removed");
    }
    Ok(())
}

pub fn credential_issue(
    name: &str,
    class: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let wire = BindingCredentialClass::from_wire(class)
        .ok_or_else(|| {
            invalid(format!(
                "`{class}` is not a credential class (announce, proxy-access)"
            ))
        })?
        .as_str();
    let path = format!("/v0/bindings/{name}/credentials");
    let routed = route("POST", &path, Some(json!({ "class": wire }))).map_err(routed_error)?;
    let answer = routed.value.unwrap_or(Value::Null);
    if json {
        let mut document = answer.clone();
        document["schema_version"] = json!(BINDING_DOCUMENT_SCHEMA_VERSION);
        println!("{}", serde_json::to_string_pretty(&document).unwrap());
        return Ok(());
    }
    // The token's one delivery: stdout, with the never-again note on stderr.
    println!("{}", answer["token"].as_str().unwrap_or_default());
    eprintln!(
        "This credential is shown once and is not stored. Nothing accepts it yet; the \
         release that does will say so."
    );
    Ok(())
}

pub fn credential_revoke(name: &str, class: &str) -> Result<(), Box<dyn std::error::Error>> {
    let wire = BindingCredentialClass::from_wire(class)
        .ok_or_else(|| {
            invalid(format!(
                "`{class}` is not a credential class (announce, proxy-access)"
            ))
        })?
        .as_str();
    let path = format!("/v0/bindings/{name}/credentials/revoke");
    route("POST", &path, Some(json!({ "class": wire }))).map_err(routed_error)?;
    println!("{name}: {wire} credential revoked");
    Ok(())
}

pub fn credential_list(name: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let credentials = engine
        .blocking()
        .binding_credentials(name)
        .map_err(engine_error)?;
    if json {
        let document = json!({
            "schema_version": BINDING_DOCUMENT_SCHEMA_VERSION,
            "credentials": credentials,
        });
        println!("{}", serde_json::to_string_pretty(&document).unwrap());
        return Ok(());
    }
    if credentials.is_empty() {
        println!("{name}: no credentials issued");
        return Ok(());
    }
    for credential in &credentials {
        println!(
            "{name}: {} {} ({}, issued {}{})",
            credential.token_id,
            credential.class.as_str(),
            credential.state,
            credential.issued_at,
            credential
                .revoked_at
                .as_ref()
                .map(|at| format!(", revoked {at}"))
                .unwrap_or_default(),
        );
    }
    println!("no token is shown again; revoke by class with `credential revoke`");
    Ok(())
}

/// One binding's human line (shared by every subcommand).
fn print_summary(binding: &Value, base_url: Option<&str>) {
    let name = binding["name"].as_str().unwrap_or_default();
    let prefix = binding["prefix"].as_str().unwrap_or_default();
    let upstream = binding["upstream"].as_str().unwrap_or_default();
    let labels = binding["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .map(|pair| {
                    let key = pair[0].as_str().unwrap_or_default();
                    let value = pair[1].as_str().unwrap_or_default();
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let labels = if labels.is_empty() {
        String::new()
    } else {
        format!(" [{}]", labels.join(","))
    };
    let base = base_url
        .map(|url| format!("  client base URL {url}"))
        .unwrap_or_default();
    println!("{name}: {prefix} -> {upstream}{labels}{base}");
}

fn print_document(key: &str, value: &Value) {
    let document = json!({
        "schema_version": BINDING_DOCUMENT_SCHEMA_VERSION,
        key: value,
    });
    println!("{}", serde_json::to_string_pretty(&document).unwrap());
}
