//! Built-in slash commands — port of
//! `packages/coding-agent/src/core/slash-commands.ts` @ pi 0.82.1 (2efa728).
//!
//! T12-S5b: the 22 built-in commands plus the hidden `/debug`. The dispatch
//! chain lives in `modes/interactive/commands.rs` /
//! `commands_selectors.rs`; this module is the single source for the
//! autocomplete list and the name/description table.

/// `SlashCommand` (slash-commands.ts:13-18) — the interactive-mode slice
/// used for autocomplete.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

const fn command(name: &'static str, description: &'static str) -> SlashCommand {
    SlashCommand {
        name,
        description,
        argument_hint: None,
    }
}

const fn command_with_hint(
    name: &'static str,
    description: &'static str,
    argument_hint: &'static str,
) -> SlashCommand {
    SlashCommand {
        name,
        description,
        argument_hint: Some(argument_hint),
    }
}

/// `BUILTIN_SLASH_COMMANDS` (slash-commands.ts:20-41) — the 22 built-in
/// commands in upstream declaration order.
pub const BUILTIN_SLASH_COMMANDS: &[SlashCommand] = &[
    command("settings", "Open settings menu"),
    command_with_hint(
        "model",
        "Select model (opens selector UI)",
        "<provider/model>",
    ),
    command("scoped-models", "Enable/disable models for Ctrl+P cycling"),
    command(
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
    ),
    command("import", "Import and resume a session from a JSONL file"),
    command("share", "Share session as a secret GitHub gist"),
    command("copy", "Copy last agent message to clipboard"),
    command("name", "Set session display name"),
    command("session", "Show session info and stats"),
    command("changelog", "Show changelog entries"),
    command("hotkeys", "Show all keyboard shortcuts"),
    command("fork", "Create a new fork from a previous user message"),
    command(
        "clone",
        "Duplicate the current session at the current position",
    ),
    command("tree", "Navigate session tree (switch branches)"),
    command("trust", "Save project trust decision for future sessions"),
    command_with_hint("login", "Configure provider authentication", "<provider>"),
    command("logout", "Remove provider authentication"),
    command("new", "Start a new session"),
    command("compact", "Manually compact the session context"),
    command("resume", "Resume a different session"),
    command(
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
    ),
    command("quit", "Quit rpi"),
];

/// Whether `name` is a built-in command (used by the extension-command
/// conflict diagnostic, interactive-mode.ts:530-543).
pub fn is_builtin_command(name: &str) -> bool {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .any(|command| command.name == name)
}
