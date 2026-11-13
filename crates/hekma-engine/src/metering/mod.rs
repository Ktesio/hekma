//! Engine-observed metering (spine AD-7 / AD-26, FR-19 `engine-observed`
//! half): the shared Metering Proxy (story 23-1) and the OpenAI-compatible
//! `usage` parse (story 3-4).
//!
//! * [`proxy`] — one loopback proxy per data-path owner: per-call
//!   identification over the published snapshot, `proxy-wire v1`, the
//!   `block` disposition over the ancestor chain, and per-call records whose
//!   close job commits the usage row and evaluates the budgets in the store
//!   writer (AD-7: one pipeline).
//! * [`relay`] — the TLS-capable forward client, body caps, the hop-by-hop
//!   strip and the `stream_options` injection.
//! * [`parse`] — the usage parse seams (JSON and SSE).
//!
//! ## Engine-INTERNAL (AD-2)
//!
//! The CLI never sees the proxy; it reads Run usage through the listing and
//! the proxy URL through the engine facade.
//!
//! ## Portable — NO OS-conditional compilation
//!
//! Async TCP + HTTP over loopback is portable tokio/`hyper` — identical on
//! Linux, macOS and Windows, with no per-OS `#[cfg]` gate anywhere in this
//! module, so it lives in the engine core and the OS-cfg grep gate stays
//! green. The proxy carries the agent's API key upstream and leaks none of it
//! into the ledger, records, logs or errors.

pub(crate) mod parse;
pub(crate) mod proxy;
pub(crate) mod relay;
