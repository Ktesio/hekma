//! A Shim program for the engine's tests (story 24-1): run as
//! `shim_host shim --listen <socket> --expect-secret-hash <hex> --launch-id <id>`,
//! it runs the engine's Shim entry exactly as `hekma shim` does, so an
//! engine opened with `EngineOptions::shim_exe` pointing here launches its
//! agents through real Shim processes.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    let (Some("shim"), Some(listen), Some(hash)) = (
        args.first().map(String::as_str),
        value("--listen"),
        value("--expect-secret-hash"),
    ) else {
        eprintln!(
            "usage: shim_host shim --listen <socket> --expect-secret-hash <hex> --launch-id <id>"
        );
        std::process::exit(2);
    };
    match hekma_engine::shim::run(std::path::Path::new(&listen), &hash) {
        Ok(_) => {}
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}
