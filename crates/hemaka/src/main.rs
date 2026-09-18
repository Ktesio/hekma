//! The `hemaka` binary (primary name). The whole CLI implementation lives
//! in the crate library so `maka` (src/bin/maka.rs) is the same program.

#[cfg(not(tarpaulin_include))]
fn main() {
    if let Err(err) = hemaka::run() {
        hemaka::handle_error_and_exit(err);
    }
}
