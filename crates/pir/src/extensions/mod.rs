//! Port of `packages/coding-agent/src/extensions/index.ts` @ pi 0.82.1
//! (2efa728) — built-in extensions.
//!
//! Upstream `builtInExtensions` holds a single hidden inline extension,
//! `llama.cpp`, loaded through the extension host (T15 in pir). Until that
//! host lands, this module is the registration seam (D-047):
//!
//! - The provider half drains into the model runtime at session-services
//!   creation (`create_agent_session_services`), mirroring
//!   agent-session-services.ts:166-178 `pendingNativeProviderRegistrations`
//!   — upstream `pi.registerProvider(providerObject)` becomes
//!   `ModelRuntime::register_native_provider`.
//! - The `/llama` command is listed in [`BUILT_IN_EXTENSION_COMMANDS`];
//!   the interactive-mode slash dispatch consults
//!   [`built_in_extension_command`] after the built-in commands miss, and
//!   the autocomplete lists it as an extension command
//!   (interactive-mode.ts:599-608).

pub mod llama;

/// A built-in (hidden) extension slash command. Upstream these register via
/// `pi.registerCommand`; here the interactive dispatch consults this table
/// (the extension-host command registry is T15).
pub struct BuiltInExtensionCommand {
    /// Invocation name (upstream `registerCommand(name, …)`).
    pub name: &'static str,
    /// Autocomplete description (upstream `RegisteredCommand.description`).
    pub description: &'static str,
}

/// The built-in extension command table — currently just the llama.cpp
/// extension's `/llama` (extensions/index.ts `builtInExtensions`).
pub const BUILT_IN_EXTENSION_COMMANDS: &[BuiltInExtensionCommand] = &[BuiltInExtensionCommand {
    name: "llama",
    description: "Manage llama.cpp router models",
}];

/// Look up a built-in extension command by invocation name.
pub fn built_in_extension_command(name: &str) -> Option<&'static BuiltInExtensionCommand> {
    BUILT_IN_EXTENSION_COMMANDS
        .iter()
        .find(|command| command.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream: "registers a native provider and /llama command"
    /// (llama-extension.test.ts) — the command-registration half; the
    /// provider registration is covered by the agent-session-services
    /// wiring and the provider tests.
    #[test]
    fn registers_llama_command_with_upstream_description() {
        let command = built_in_extension_command("llama").expect("llama command");
        assert_eq!(command.description, "Manage llama.cpp router models");
        assert!(built_in_extension_command("model").is_none());
        assert!(!crate::core::slash_commands::is_builtin_command("llama"));
    }
}
