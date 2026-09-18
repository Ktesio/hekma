# ktesio-adapter-api (DEPRECATED — renamed to hekma-adapter-api)

This crate was renamed as part of the v0.8.0 Ktesio → Hekma product
rename (2026-09-16). **No other version will ever be published under
this name.**

- New crate: [hekma-adapter-api](https://crates.io/crates/hekma-adapter-api)
- Migration guide: <https://hekma.ktesio.dev/migration>

This final release (0.1.1) re-exports `hekma-adapter-api`, so existing
`use hekma_adapter_api::…` code keeps compiling after `cargo update` — the
re-exported types ARE the new crate's types (one crate instance, no
duplication). Nothing about your data or environment changes.

```toml
[dependencies]
hekma-adapter-api = "0.2"
```
