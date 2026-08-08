//! Extension shortcut dispatch for interactive mode (T15 W3) —
//! `defaultEditor.onExtensionShortcut` wiring
//! (interactive-mode.ts:1827-1841).
//!
//! The editor consults the hook before its own key handling; conflict
//! resolution (reserved built-ins, last-wins across extensions, override
//! warnings) lives in the host (`getShortcuts`, runner.ts:490-533). Handler
//! errors surface via `showError` (interactive-mode.ts:1835-1837).

use std::sync::Arc;

use pir_ext_host::host::NativeExtensionHost;

use super::interactive_mode::commands_selectors::spawn_async;
use super::interactive_mode::InteractiveUi;
use crate::core::agent_session::AgentSession;

/// Resolved built-in keybindings as `(action_id, keys)` pairs — the input
/// shape of the host's `getShortcuts` (`KeybindingsConfig`, keys split into
/// lists; runner.ts:92-111 normalizes case itself).
fn builtin_keybinding_pairs() -> Vec<(String, Vec<String>)> {
    let manager = crate::core::keybindings::get_keybindings()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    manager
        .get_resolved_bindings()
        .into_iter()
        .map(|(id, value)| (id, value.to_vec()))
        .collect()
}

/// Reach the host through the session's extension runner (no-op for the
/// no-op runner / non-host runners).
fn host_of(session: &AgentSession) -> Option<Arc<NativeExtensionHost>> {
    crate::core::extension_host_adapter::host_of_runner(&session.extension_runner())
}

/// Install the editor hook. `interactive-mode.ts:1828-1841`.
pub(crate) fn install_extension_shortcuts(ui: &Arc<InteractiveUi>, session: &AgentSession) {
    let Some(host) = host_of(session) else {
        return;
    };
    let shortcuts = host.get_shortcuts(&builtin_keybinding_pairs());
    if shortcuts.is_empty() {
        return;
    }
    lock_editor_hook(ui, host, shortcuts);
}

fn lock_editor_hook(
    ui: &Arc<InteractiveUi>,
    host: Arc<NativeExtensionHost>,
    shortcuts: pir_ext_host::api::InsertionMap<pir_ext_host::types::ExtensionShortcut>,
) {
    let ui_weak = Arc::downgrade(ui);
    lock(&ui.editor).on_extension_shortcut = Some(Box::new(move |data: &str| {
        for (key, shortcut) in shortcuts.iter() {
            if pir_tui::keys::matches_key(data, key) {
                let handler = shortcut.handler.clone();
                let host = host.clone();
                let ui_weak = ui_weak.clone();
                // Run the handler async, don't block input
                // (interactive-mode.ts:1833-1838).
                spawn_async(async move {
                    let ctx = host.core().create_context();
                    if let Err(error) = handler(ctx).await {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.show_error(&format!("Shortcut handler error: {error}"));
                        }
                    }
                });
                return true;
            }
        }
        false
    }));
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
