//! V19-00 headless replay development binary; not a distributed CLI surface.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn arena_heap_count() -> Result<usize, String> {
    unsafe {
        let mut buffer = std::ptr::null_mut();
        let mut length = 0;
        let stream = libc::open_memstream(&mut buffer, &mut length);
        if stream.is_null() {
            return Err("open_memstream failed".to_owned());
        }
        let result = libc::malloc_info(0, stream);
        libc::fclose(stream);
        if result != 0 {
            if !buffer.is_null() {
                libc::free(buffer.cast());
            }
            return Err("malloc_info failed".to_owned());
        }
        if buffer.is_null() {
            return Err("malloc_info returned no buffer".to_owned());
        }
        let bytes = std::slice::from_raw_parts(buffer.cast::<u8>(), length);
        let info = String::from_utf8_lossy(bytes).into_owned();
        libc::free(buffer.cast());
        Ok(info.matches("<heap nr=").count())
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn run_arena_probe() -> Result<usize, String> {
    use std::process::Command;
    use std::sync::{Arc, Barrier};

    if std::env::var_os(rpi::core::early_init::ENV_MALLOC_ARENA_MAX).is_none() {
        let status = Command::new("/bin/sh")
            .args(["-c", "test -z \"${MALLOC_ARENA_MAX+x}\""])
            .status()
            .map_err(|error| format!("spawn environment child: {error}"))?;
        if !status.success() {
            return Err("default arena limit leaked into the child environment".to_owned());
        }
    }

    const WORKERS: usize = 16;
    let start = Arc::new(Barrier::new(WORKERS + 1));
    let allocated = Arc::new(Barrier::new(WORKERS + 1));
    let mut workers = Vec::new();
    for worker in 0..WORKERS {
        let start = Arc::clone(&start);
        let allocated = Arc::clone(&allocated);
        workers.push(
            std::thread::Builder::new()
                .spawn(move || {
                    start.wait();
                    let allocations: Vec<Vec<u8>> = (0..64)
                        .map(|index| vec![(worker + index) as u8; 16 * 1024])
                        .collect();
                    allocated.wait();
                    drop(allocations);
                })
                .map_err(|error| format!("spawn allocation worker: {error}"))?,
        );
    }
    start.wait();
    allocated.wait();
    for worker in workers {
        worker
            .join()
            .map_err(|_| "allocation worker panicked".to_owned())?;
    }
    arena_heap_count()
}

fn main() {
    rpi::core::early_init::initialize();

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("RPI_V19_ARENA_PROBE").is_some() {
        match run_arena_probe() {
            Ok(heaps) => {
                println!("{heaps}");
                return;
            }
            Err(error) => {
                eprintln!("arena probe failed: {error}");
                std::process::exit(1);
            }
        }
    }

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
