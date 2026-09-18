//! The `hekma` binary (primary name). The whole CLI implementation lives
//! in the crate library so `hkm` (src/bin/hkm.rs) is the same program.

#[cfg(not(tarpaulin_include))]
fn main() {
    if let Err(err) = hekma::run() {
        hekma::handle_error_and_exit(err);
    }
}
