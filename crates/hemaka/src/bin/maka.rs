//! The `maka` binary — the short alias for `hemaka`. Both names run the
//! same CLI implementation (the crate library); `maka --version` reports
//! the shared `hemaka` identity so version checks match either binary.

#[cfg(not(tarpaulin_include))]
fn main() {
    if let Err(err) = hemaka::run() {
        hemaka::handle_error_and_exit(err);
    }
}
