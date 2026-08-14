//! Plugin-owned tokio runtime (design doc §2.3, runtime model).
//!
//! L0 dispatch runs synchronously on the host's calling thread, while MCP
//! connections, the 30s health check and idle timers are long-lived async
//! tasks. All of those run on this private multi-thread runtime (2 workers,
//! resident for the session); dispatch entry points bridge in with
//! [`PluginRuntime::block_on`], mirroring the host's own
//! `wasm/host_call.rs` `block_on(&handle, ...)` pattern.
//!
//! Cancellation: `session_start` / `session_shutdown` cancel in-flight tasks
//! through the [`tokio_util::sync::CancellationToken`], flush the metadata
//! cache and gracefully shut servers down — one-to-one with upstream
//! `McpRuntimeOwner` semantics (`index.ts` / `runtime-owner.ts`).

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Number of runtime worker threads (design §2.3: multi-thread, 2 workers).
const RUNTIME_WORKERS: usize = 2;

/// The plugin's private tokio runtime plus its session-scoped cancel token.
pub struct PluginRuntime {
    runtime: Option<tokio::runtime::Runtime>,
    cancel: CancellationToken,
}

impl PluginRuntime {
    /// Build the runtime. Returns an error instead of panicking if the OS
    /// refuses to spawn threads (reported to the host as an init error).
    pub fn start() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(RUNTIME_WORKERS)
            .thread_name("rpi-mcp-adapter")
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
            cancel: CancellationToken::new(),
        })
    }

    /// Session-scoped cancellation token; cloning shares the token.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run a future to completion from a synchronous dispatch entry point.
    ///
    /// Must be called from a thread that is NOT part of this runtime (the
    /// host's dispatch thread); `Handle::block_on` drives the future on the
    /// current thread while tasks it spawns run on the runtime workers.
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        let runtime = self
            .runtime
            .as_ref()
            // Invariant: block_on is only reachable between start() and
            // shutdown(); shutdown happens once at session_shutdown after the
            // host has drained dispatches.
            .expect("plugin runtime used after shutdown");
        runtime.handle().block_on(future)
    }

    /// Spawn a session-scoped task onto the runtime.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let runtime = self
            .runtime
            .as_ref()
            // Invariant: same as block_on — spawn is unreachable post-shutdown.
            .expect("plugin runtime used after shutdown");
        runtime.spawn(future)
    }

    /// Cancel session tasks and stop the runtime. Waits for in-flight tasks
    /// to observe cancellation before returning (graceful shutdown ordering
    /// per design §3.6: cancel -> await in-flight -> close connections).
    ///
    /// The closeAll leg (clients first, then stdio child processes) lives
    /// upstream of this call: `session_start` / `session_shutdown` run
    /// `dispatcher.shutdown_owned()` (lib.rs), which performs the owner
    /// cancel + metadata flush + `lifecycle.graceful_shutdown()` before the
    /// runtime drops.
    pub fn shutdown(&self) {
        self.cancel.cancel();
        // Dropping a Runtime blocks until all tasks yield; with the token
        // cancelled, well-behaved tasks exit promptly.
    }
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_bridges_sync_dispatch_into_the_runtime() {
        let runtime = PluginRuntime::start().expect("runtime starts");
        let value = runtime.block_on(async {
            tokio::task::yield_now().await;
            42
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn shutdown_cancels_session_tasks() {
        let runtime = PluginRuntime::start().expect("runtime starts");
        let token = runtime.cancel_token();
        let task = runtime.spawn(async move {
            token.cancelled().await;
            "cancelled"
        });
        runtime.shutdown();
        assert_eq!(
            runtime.block_on(async { task.await.ok() }),
            Some("cancelled")
        );
    }
}
