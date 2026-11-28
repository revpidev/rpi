//! `rpi` binary entry point (T10: headless modes; interactive lands in T12).

fn main() {
    rpi::core::early_init::initialize();

    // V19-09 hook #1 (ADR-0050, Windows only): the startup self-update —
    // the pending transaction, the cleanup scan and, when this process
    // just applied an update, the trampoline wait for the new instance.
    // The wait never returns: the previous version never starts a second
    // user interface on the same console.
    #[cfg(windows)]
    match rpi::core::windows_update::startup::before_run(
        &std::env::args_os().skip(1).collect::<Vec<_>>(),
    ) {
        rpi::core::windows_update::startup::Startup::Run => {}
        rpi::core::windows_update::startup::Startup::Trampoline(trampoline) => {
            trampoline.wait_and_exit()
        }
    }

    // Marker env for child processes/extensions (cli.ts:13-14).
    // SAFETY: process start; the tokio runtime and its worker threads do
    // not exist yet, so no concurrent env readers can race this write.
    unsafe { std::env::set_var("RPI_CODING_AGENT", "true") };
    rpi::core::environment::set_ai_agent_marker();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: failed to start runtime: {error}");
            rpi::process_exit::exit_fatal(1);
        }
    };
    let exit_code = runtime.block_on(rpi::app::run_app(args));
    // V19-09 hook #2: the unified exit outlet marks the normal exit
    // (Windows); elsewhere it is the plain process exit.
    #[cfg(windows)]
    rpi::process_exit::exit(exit_code);
    #[cfg(not(windows))]
    std::process::exit(exit_code);
}
