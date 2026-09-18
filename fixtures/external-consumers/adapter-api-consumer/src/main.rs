//! External-consumer fixture: the Adapter Contract surface (manifest
//! parsing + validation + projections) must compile and behave
//! identically against the baseline crate (ktesio-adapter-api at the
//! contract-v1 freeze) and the renamed crate (hekma-adapter-api).
//!
//! Consumed by scripts/rename_surface_check.sh, which builds this file
//! against BOTH sources with the dependency renamed to the neutral
//! `engine` alias — so this fixture is written against the alias, never
//! against either concrete crate name.

use engine::{ConfigMapping, Manifest, MeteringSource};

const ADAPTER_TOML: &str = r#"
contract_version = "1.0.0"

[adapter]
kind = "fixture"

[lifecycle.start]
exec = "/usr/bin/true"
args = []

[capabilities.pause]
linux = "guaranteed"
macos = "best-effort"

[metering]
source = "self-reported"
"#;

fn main() {
    let manifest = Manifest::from_toml_str(ADAPTER_TOML).expect("parse the fixture manifest");
    manifest.validate().expect("validate the fixture manifest");

    // The projection surface a host integrates against.
    let metering: Option<MeteringSource> = manifest.metering_source();
    assert!(metering.is_some(), "self-reported metering must project");
    assert_eq!(manifest.adapter_kind(), Some("fixture"));
    let _mapping: ConfigMapping = manifest.config_mapping();

    println!("adapter-api consumer ok");
}
