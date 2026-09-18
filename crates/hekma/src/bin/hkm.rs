//! The `hkm` binary — the short alias for `hekma`. Both names run the
//! same CLI implementation (the crate library); `hkm --version` reports
//! the shared `hekma` identity so version checks match either binary.

#[cfg(not(tarpaulin_include))]
fn main() {
    if let Err(err) = hekma::run() {
        hekma::handle_error_and_exit(err);
    }
}
