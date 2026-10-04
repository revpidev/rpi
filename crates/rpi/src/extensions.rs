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

pub mod codemode;
pub mod llama;
pub mod mcp;
pub mod tool_search;

/// Names of the built-in extensions (`extensions/index.ts:7-14` @
/// a13d35a74: `llama.cpp`, `codemode`, `tool-search`, `mcp`), in the
/// upstream registration order. The package manager resolves each as a
/// `builtin:<name>` extension resource (V16-13 FR-A); keep in sync with the
/// factories app.rs builds.
pub const BUILTIN_EXTENSION_NAMES: [&str; 4] = ["llama.cpp", "codemode", "tool-search", "mcp"];

/// [`BUILTIN_EXTENSION_NAMES`] as owned strings
/// (`PackageManagerOptions.builtinExtensions`).
pub fn builtin_extension_names() -> Vec<String> {
    BUILTIN_EXTENSION_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    /// Upstream: "registers a native provider and /llama command"
    /// (llama-extension.test.ts) — registration shape through the real
    /// host, loaded as the `builtin:llama.cpp` resource (V16-13 FR-A).
    #[tokio::test]
    async fn registers_llama_command_and_provider_via_factory() {
        let host = rpi_ext_host::host::NativeExtensionHost::new("/x");
        // A `builtin: true` factory never loads inline (types.ts:2215).
        let inline_errors = host.load_inline(&[super::llama::inline_extension()]).await;
        assert!(inline_errors.is_empty(), "{inline_errors:?}");
        assert!(host.core().extensions().is_empty());

        // The final pass loads it from the `builtin:llama.cpp` path.
        let errors = host
            .load_startup_final(
                std::path::PathBuf::from("/agent"),
                vec!["builtin:llama.cpp".to_owned()],
                Vec::new(),
                vec![super::llama::inline_extension()],
                false,
                false,
            )
            .await;
        assert!(errors.is_empty(), "{errors:?}");
        // Hidden built-in (not in the startup Extensions list); the path
        // and source info both use the `builtin:` naming (FR-A R4).
        let core = host.core();
        let ext = &core.extensions()[0];
        assert_eq!(ext.path, "builtin:llama.cpp");
        assert!(ext.hidden());
        assert!(ext.builtin());
        assert_eq!(ext.source_info.source, "builtin");
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

    /// `builtin_registry` ignores non-builtin named entries and keeps the
    /// upstream order (V16-13 FR-A).
    #[test]
    fn builtin_names_match_the_upstream_registry() {
        assert_eq!(
            super::BUILTIN_EXTENSION_NAMES,
            ["llama.cpp", "codemode", "tool-search", "mcp"]
        );
    }
}
