//! Port of `packages/coding-agent/src/extensions/index.ts` @ pi 0.82.1
//! (2efa728) — built-in extensions.
//!
//! Upstream `builtInExtensions` holds a single hidden inline extension,
//! `llama.cpp`, loaded through the extension host. T15 W7 closed the D-047
//! seam: [`llama::inline_extension`] returns the `InlineExtension::Named`
//! (`hidden: true`) factory the startup pipeline (app.rs `create_runtime`)
//! loads through the real host:
//!
//! - The provider registers via `pi.registerProvider(providerObject)` →
//!   `HostActions::register_native_provider`; app.rs flushes the pending
//!   native queue into the model runtime before session creation
//!   (agent-session-services.ts:166-178 equivalent).
//! - `/llama` registers via `pi.registerCommand` and dispatches through
//!   `session.prompt`'s extension-command path, like upstream.
//! - The manager UI mounts its native TUI view through the interactive
//!   bridge's L0 escape hatch (`InteractiveUiBridge::interactive_ui`).

pub mod llama;

#[cfg(test)]
mod tests {
    /// Upstream: "registers a native provider and /llama command"
    /// (llama-extension.test.ts) — registration shape through the real
    /// host.
    #[tokio::test]
    async fn registers_llama_command_and_provider_via_factory() {
        let host = rpi_ext_host::host::NativeExtensionHost::new("/x");
        let errors = host.load_inline(&[super::llama::inline_extension()]).await;
        assert!(errors.is_empty(), "{errors:?}");
        // Hidden built-in (not in the startup Extensions list).
        let core = host.core();
        let ext = &core.extensions()[0];
        assert_eq!(ext.path, "<inline:llama.cpp>");
        assert!(ext.hidden());
        // Command registered with the upstream description.
        let command = host.get_command("llama").expect("llama command");
        assert_eq!(
            command.description.as_deref(),
            Some("Manage llama.cpp router models")
        );
        // Provider queued for the pre-bind flush.
        let pending = host.runtime().take_pending_native_provider_registrations();
        assert_eq!(pending.len(), 1);
    }
}
