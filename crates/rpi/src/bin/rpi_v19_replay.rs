//! V19-00 headless replay development binary; not a distributed CLI surface.

fn main() {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to create replay runtime: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) =
        runtime.block_on(rpi::modes::interactive::interactive_mode::v19_replay::run_from_env())
    {
        eprintln!("replay failed: {error}");
        std::process::exit(1);
    }
}
