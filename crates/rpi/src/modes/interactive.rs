//! Interactive mode — mirrors `packages/coding-agent/src/modes/interactive/`
//! @ pi 0.82.1 (2efa728).
//!
//! The message-rendering component family lives under `components/` with the
//! `theme.rs` helpers they consume; `interactive_mode.rs` carries the mode
//! skeleton (layout, event dispatch, run loop), `footer.rs`,
//! `custom_editor.rs` and `header.rs` the frame, and `commands.rs` /
//! `interactive_mode/commands_selectors.rs` the selectors and slash
//! commands. `theme_watcher.rs` / `git_branch_watcher.rs` are the polling
//! watchers; `startup_ui.rs` the first-run setup.

pub mod autocomplete;
pub mod commands;
pub mod component_tree;
pub mod components;
pub mod custom_editor;
pub mod extension_renderers;
pub(crate) mod extension_shortcuts;
pub mod external_editor;
pub mod footer;
pub(crate) mod git_branch_watcher;
pub mod header;
pub mod interactive_mode;
pub(crate) mod startup_ui;
pub mod theme;
pub(crate) mod theme_watcher;
pub mod tool_renderers;

pub use interactive_mode::{run_interactive_mode, InteractiveMode, InteractiveModeOptions};

#[cfg(test)]
mod snapshots;
#[cfg(test)]
pub(crate) mod test_support;
