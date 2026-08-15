//! Private tokio runtime for the plugin (same shape as mcp-adapter's and
//! subagents' `runtime.rs`, design §2.2): the host calls the cdylib
//! synchronously, so the async wreq pipeline runs here and dispatch blocks
//! on it. Not shared with the host runtime — no cross-runtime handles.

/// Plugin-owned runtime. Two workers since TE07: the batch worker pool
/// drives bounded-concurrent fetches through here (a single worker would
/// serialize them; more adds nothing — wreq spawns its own I/O tasks).
pub struct PluginRuntime {
    runtime: tokio::runtime::Runtime,
}

impl PluginRuntime {
    /// Build the runtime; `None` when the tokio builder refuses (treated as
    /// a load failure by the caller, mirroring the other L0 plugins).
    pub fn new() -> Option<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .ok()?;
        Some(Self { runtime })
    }

    /// Run a future to completion on the plugin runtime. Dispatch never runs
    /// on a plugin-runtime worker (the host owns its own threads), so
    /// `block_on` cannot self-deadlock (serial dispatch contract,
    /// extension-abi §2.2).
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginRuntime")
    }
}
