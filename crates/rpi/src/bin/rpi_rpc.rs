//! `rpi-rpc` binary entry point: equivalent to `rpi --mode rpc`
//! (`rpc-entry.ts` @ pi 0.82.1).

fn main() {
    // Marker env for child processes/extensions (rpc-entry.ts:7).
    // SAFETY-FREE note: set before the runtime starts; no readers race.
    std::env::set_var("RPI_CODING_AGENT", "true");

    let mut args = vec!["--mode".to_owned(), "rpc".to_owned()];
    args.extend(std::env::args().skip(1));
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
    let exit_code = runtime.block_on(rpi::app::run_app(args));
    std::process::exit(exit_code);
}
