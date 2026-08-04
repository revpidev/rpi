//! `pir` binary entry point (T10: headless modes; interactive lands in T12).

fn main() {
    // Marker env for child processes/extensions (cli.ts:13).
    // SAFETY-FREE note: set before the runtime starts; no readers race.
    std::env::set_var("PIR_CODING_AGENT", "true");

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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: failed to start runtime: {error}");
            std::process::exit(1);
        }
    };
    let exit_code = runtime.block_on(pir::app::run_app(args));
    std::process::exit(exit_code);
}
