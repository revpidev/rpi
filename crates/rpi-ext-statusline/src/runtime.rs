//! Plugin-owned tokio runtime (mcp-adapter `src/runtime.rs` precedent,
//! design §2.3 runtime model).
//!
//! L0 dispatch runs synchronously on the host's calling thread, while the
//! refresh loop (debounce / script execution / UI pushes) is a long-lived
//! async task. That task runs on this private multi-thread runtime
//! (2 workers, resident for the session); nothing bridges in with
//! `block_on` after install — the dispatch path only does in-memory state
//! updates plus a channel send (TE-D6 dispatch-blocking discipline).
//!
//! Cancellation: unlike mcp-adapter there is no session-scoped
//! CancellationToken — cancellation is per-script
//! ([`crate::runner::CancelToken`]) plus a generation counter in the
//! refresh loop; `session_shutdown` stops the loop and `Drop` parks the
//! runtime with `shutdown_background()`.

/// Number of runtime worker threads (mcp-adapter precedent: 2).
const RUNTIME_WORKERS: usize = 2;

/// The plugin's private tokio runtime.
pub struct PluginRuntime {
    runtime: Option<tokio::runtime::Runtime>,
}

impl PluginRuntime {
    /// Build the runtime. Returns an error instead of panicking if the OS
    /// refuses to spawn threads (reported to the host as an init error).
    pub fn start() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(RUNTIME_WORKERS)
            .thread_name("rpi-statusline")
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    /// Run a future to completion from a synchronous entry point.
    ///
    /// Must be called from a thread that is NOT part of this runtime;
    /// `Handle::block_on` drives the future on the current thread while
    /// tasks it spawns run on the runtime workers.
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        let runtime = self
            .runtime
            .as_ref()
            // Invariant: block_on is only reachable between start() and
            // drop(); the runtime is dropped once at session_shutdown
            // after the host has drained dispatches.
            .expect("plugin runtime used after shutdown");
        runtime.handle().block_on(future)
    }

    /// Spawn a background task onto the runtime.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let runtime = self
            .runtime
            .as_ref()
            // Invariant: same as block_on — spawn is unreachable post-drop.
            .expect("plugin runtime used after shutdown");
        runtime.spawn(future)
    }
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_bridges_sync_caller_into_the_runtime() {
        let runtime = PluginRuntime::start().expect("runtime starts");
        let value = runtime.block_on(async {
            tokio::task::yield_now().await;
            42
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn spawn_runs_background_tasks_to_completion() {
        let runtime = PluginRuntime::start().expect("runtime starts");
        let task = runtime.spawn(async {
            tokio::task::yield_now().await;
            "done"
        });
        let result = runtime.block_on(task);
        assert_eq!(result.ok(), Some("done"));
    }
}
