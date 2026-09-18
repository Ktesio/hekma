//! DEPRECATED: this crate was renamed to **hekma-engine** (v0.8.0 Hekma
//! rename, 2026-09-16). Depend on the new name instead:
//!
//! ```toml
//! [dependencies]
//! hekma-engine = "0.4"
//! ```
//!
//! This final `ktesio-engine` release (0.3.1) exists only to re-export
//! the new crate, so existing `use hekma_engine::…` paths keep compiling
//! after `cargo update`. No other version will ever be published under
//! this name. Migration guide: <https://hekma.ktesio.dev/migration>.

pub use hekma_engine::*;
