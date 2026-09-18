//! DEPRECATED: this crate was renamed to **hekma-adapter-api** (v0.8.0 Hekma
//! rename, 2026-09-16). Depend on the new name instead:
//!
//! ```toml
//! [dependencies]
//! hekma-adapter-api = "0.2"
//! ```
//!
//! This final `ktesio-adapter-api` release (0.1.1) exists only to re-export
//! the new crate, so existing `use hekma_adapter_api::…` paths keep compiling
//! after `cargo update`. No other version will ever be published under
//! this name. Migration guide: <https://hekma.ktesio.dev/migration>.

pub use hekma_adapter_api::*;
