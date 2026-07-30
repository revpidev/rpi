//! `pir` binary entry point (skeleton, T01).
//!
//! The real CLI (clap-based, mirroring `packages/coding-agent/src/cli/`) lands
//! with the headless/interactive mode tasks (T10/T12).

fn main() {
    // T02 measurement hook: exercises the embedded wasmtime runtime (kept
    // linked into the release binary); T15 replaces this with the real host.
    if std::env::args().any(|a| a == "--wasm-smoke") {
        match pir_ext_host::wasm_smoke() {
            Ok(info) => println!("{info}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    println!(
        "pir {} (workspace skeleton; see docs/plan/v0.1 for the implementation plan)",
        env!("CARGO_PKG_VERSION")
    );
}
