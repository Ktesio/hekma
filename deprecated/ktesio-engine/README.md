# ktesio-engine (DEPRECATED — renamed to hekma-engine)

This crate was renamed as part of the v0.8.0 Ktesio → Hekma product
rename (2026-09-16). **No other version will ever be published under
this name.**

- New crate: [hekma-engine](https://crates.io/crates/hekma-engine)
- Migration guide: <https://hekma.ktesio.dev/migration>

This final release (0.3.1) re-exports `hekma-engine`, so existing
`use hekma_engine::…` code keeps compiling after `cargo update` — the
re-exported types ARE the new crate's types (one crate instance, no
duplication). Nothing about your data or environment changes.

```toml
[dependencies]
hekma-engine = "0.4"
```
