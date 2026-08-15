//! Private tokio runtime for the plugin (same shape as mcp-adapter's
//! `runtime.rs`, design §2.3): the host calls the cdylib synchronously, so
//! async work (child process streaming) runs here and dispatch blocks on it.

/// Plugin-owned runtime. Two worker threads cover stdout/stderr streaming and
/// the signal ladders for one foreground run (parallel children arrive in
/// TE05 and will revisit sizing).
pub struct PluginRuntime {
    runtime: tokio::runtime::Runtime,
}

impl PluginRuntime {
    /// Build the runtime; `None` when the tokio builder refuses (treated as a
    /// load failure by the caller, mirroring mcp-adapter).
    pub fn new() -> Option<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .ok()?;
        Some(Self { runtime })
    }

    /// Run a future to completion on the plugin runtime. Re-entrant use from
    /// runtime threads is a programming error; dispatch is single-threaded
    /// per extension instance (serial dispatch contract, extension-abi §2.2).
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        // Invariant: dispatch never runs on a plugin-runtime worker (the host
        // owns its own threads), so block_on cannot self-deadlock.
        self.runtime.block_on(future)
    }

    /// Fire-and-forget task on the plugin runtime (async run bodies). The
    /// lifecycle paths (agent_end drain, session_shutdown harvest) use
    /// `block_on` instead — the host must observe their completion before
    /// exit, which a detached task cannot guarantee.
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginRuntime")
    }
}
