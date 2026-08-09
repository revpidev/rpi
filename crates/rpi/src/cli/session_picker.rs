//! Port of `cli/session-picker.ts` @ pi 0.82.1 (2efa728): the `--resume`
//! startup session selector — a standalone TUI shown before the session
//! manager is created (main.ts:321-333).
//!
//! Intentional differences:
//! - The component loads session lists synchronously from `cwd` /
//!   `session_dir` (see the `SessionSelectorComponent` port notes), so the
//!   loader callbacks of `selectSession` collapse into plain path arguments.
//! - The terminal is injectable for tests (the `with_terminal` variant);
//!   production uses a `ProcessTerminal`.
//! - `onExit` is a no-op: the component port never fires it (the field is
//!   kept for interface parity only).
//! - The theme watcher is not started for the picker (upstream stops it in a
//!   `finally` because the app-level watcher may already run; rpi starts the
//!   watcher with the interactive mode instead).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_tui::terminal::ProcessTerminal;
use rpi_tui::tui::{shared_component_from_boxed, Component, Focusable, Tui};

use crate::core::settings_manager::SettingsManager;
use crate::core::themes::load_theme;
use crate::error::PirError;
use crate::modes::interactive::components::session_selector::SessionSelectorComponent;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// TUI entry wrapper for the picker: forwards component calls and focus to
/// the shared component (same idiom as `FocusableRegion` in
/// `interactive_mode.rs`, which is module-private).
struct PickerRegion(Arc<Mutex<SessionSelectorComponent>>);

impl Component for PickerRegion {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.0).render(width)
    }

    fn handle_input(&mut self, data: &str) {
        lock(&self.0).handle_input(data);
    }

    fn invalidate(&mut self) {
        lock(&self.0).invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for PickerRegion {
    fn focused(&self) -> bool {
        lock(&self.0).focused()
    }

    fn set_focused(&mut self, focused: bool) {
        lock(&self.0).set_focused(focused);
    }
}

/// `selectSession` (session-picker.ts:15-57) on the process terminal.
/// Returns the selected session file path, or `None` when cancelled.
pub(crate) async fn select_session(
    cwd: &Path,
    session_dir: Option<&Path>,
    settings: &SettingsManager,
) -> Result<Option<String>, PirError> {
    select_session_with_terminal(cwd, session_dir, settings, Box::new(ProcessTerminal::new())).await
}

/// `selectSession` on an injected terminal (test injection point).
pub(crate) async fn select_session_with_terminal(
    cwd: &Path,
    session_dir: Option<&Path>,
    settings: &SettingsManager,
    terminal: Box<dyn rpi_tui::terminal::Terminal + Send>,
) -> Result<Option<String>, PirError> {
    // The component's input matching reads the global keybinding tables
    // (session-picker.ts:24-25 `setKeybindings`).
    crate::modes::interactive::interactive_mode::install_global_keybindings();

    // `createStartupTui` (startup-ui.ts:77-85): theme from settings, then a
    // TUI over the process terminal.
    let theme_name = settings.get_theme().unwrap_or_else(|| "dark".to_string());
    let theme = load_theme(&theme_name, None)
        .or_else(|_| load_theme("dark", None))
        .map_err(|error| PirError::Resource(error.to_string()))?;
    let ui = Tui::with_options(
        terminal,
        Some(settings.get_show_hardware_cursor()),
        Some(crate::config::get_agent_dir()),
    );
    ui.set_clear_on_shrink(settings.get_clear_on_shrink());

    let (select_tx, select_rx) = tokio::sync::oneshot::channel::<String>();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let mut select_tx = Some(select_tx);
    let mut cancel_tx = Some(cancel_tx);
    let selector = Arc::new(Mutex::new(SessionSelectorComponent::new(
        cwd.to_path_buf(),
        session_dir.map(Path::to_path_buf),
        Arc::new(theme),
        ui.clone(),
        Box::new(move |path: &str| {
            if let Some(tx) = select_tx.take() {
                let _ = tx.send(path.to_string());
            }
        }),
        Box::new(move || {
            if let Some(tx) = cancel_tx.take() {
                let _ = tx.send(());
            }
        }),
        // `onExit` (session-picker.ts:40-43) stops the UI and exits; the
        // component port never fires it, so a no-op suffices here.
        Box::new(|| {}),
        // No delete / rename in the startup picker (session-picker.ts passes
        // neither; `showRenameHint: false`).
        None,
        None,
        // No running session yet — nothing to protect from deletion.
        None,
    )));

    let entry = shared_component_from_boxed(Box::new(PickerRegion(selector)));
    ui.add_child(entry.clone());
    ui.set_focus(Some(entry));
    ui.request_render(false);

    // `startStartupTui` (startup-ui.ts:87-90): start LAST
    // (session-picker.ts:53-55 mounts + focuses before starting) so no input
    // can arrive before the selector is focused. The auto-theme detection of
    // `applyDetectedStartupTheme` stays with the interactive mode.
    ui.start();
    let stop = Arc::new(AtomicBool::new(false));
    let driver_ui = ui.clone();
    let driver_stop = Arc::clone(&stop);
    let driver = std::thread::Builder::new()
        .name("rpi-picker-driver".to_string())
        .spawn(move || {
            while !driver_stop.load(Ordering::Relaxed) {
                driver_ui.pump(Some(Duration::from_millis(50)));
            }
        })
        .map_err(PirError::Io)?;

    let selected = tokio::select! {
        path = select_rx => path.ok(),
        _ = cancel_rx => None,
    };

    stop.store(true, Ordering::Relaxed);
    let _ = driver.join();
    ui.stop();

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::core::settings_manager::SettingsManagerCreateOptions;
    use crate::modes::interactive::test_support::{TempDir, TestTerminal};

    /// Build the picker's inputs over a temp agent dir / cwd, with one
    /// persisted session file in `session_dir` (written directly —
    /// `SessionManager` defers file creation to the first assistant
    /// message, session-manager.ts:1015-1042).
    fn picker_fixture() -> (TempDir, PathBuf, PathBuf, SettingsManager, String) {
        let tmp = TempDir::new();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("cwd");
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&session_dir).expect("sessions dir");
        let settings = SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            SettingsManagerCreateOptions::default(),
        );
        let session_path = session_dir.join("s1.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "s1",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd.to_string_lossy(),
        });
        let message = serde_json::json!({
            "type": "message",
            "id": "m-s1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "hello"}],
                "timestamp": 1_700_000_000_000i64,
            },
        });
        std::fs::write(&session_path, format!("{header}\n{message}\n")).expect("write session");
        let session_file = session_path.to_string_lossy().into_owned();
        (tmp, cwd, session_dir, settings, session_file)
    }

    /// Feed `data` once the TUI has installed its input handler.
    async fn feed_when_started(terminal: &TestTerminal, data: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !terminal.is_started() {
            assert!(std::time::Instant::now() < deadline, "TUI never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        terminal.feed(data);
    }

    #[tokio::test]
    async fn escape_cancels_picker() {
        let (_tmp, cwd, session_dir, settings, _file) = picker_fixture();
        let terminal = TestTerminal::new();
        let picker = select_session_with_terminal(
            &cwd,
            Some(&session_dir),
            &settings,
            Box::new(terminal.clone()),
        );
        let feeder = feed_when_started(&terminal, "\x1b");
        let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(picker, feeder)
        })
        .await
        .expect("picker timed out");
        assert_eq!(result.expect("picker result"), None);
    }

    #[tokio::test]
    async fn enter_selects_highlighted_session() {
        let (_tmp, cwd, session_dir, settings, session_file) = picker_fixture();
        let terminal = TestTerminal::new();
        let picker = select_session_with_terminal(
            &cwd,
            Some(&session_dir),
            &settings,
            Box::new(terminal.clone()),
        );
        let feeder = feed_when_started(&terminal, "\r");
        let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(picker, feeder)
        })
        .await
        .expect("picker timed out");
        assert_eq!(result.expect("picker result"), Some(session_file));
    }
}
