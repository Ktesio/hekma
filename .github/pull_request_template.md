## Summary

-

## Verification

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `python3 scripts/check_docs.py`

Note: a bare `cargo` runs the repo's MSRV pin (`rust-toolchain.toml` → Rust 1.96.1), while CI's latest-stable jobs run `cargo +stable`; version managers (mise/asdf) can override the pin via `RUSTUP_TOOLCHAIN` — see docs/testing.md ("Toolchain").

## Contributor Checklist

- [ ] I kept the change focused.
- [ ] I updated tests or docs where behavior changed.
- [ ] I have read and agree to the [Contributor License Agreement](https://github.com/iMagdy/ktesio/blob/main/CLA.md).
- [ ] Security-sensitive changes were reviewed with extra care.
