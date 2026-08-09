//! Port of `cli/startup-ui.ts` @ pi 0.82.1 (2efa728): the first-time setup
//! dialog shown before the interactive mode when no settings file exists yet
//! (main.ts:563-565, startup-ui.ts:115-132, 166-205).
//!
//! Intentional differences:
//! - `isOfficialDistribution` (startup-ui.ts:36-42) is trivially true: the
//!   rpi binary is the official distribution of itself.
//! - The theme registry (`setRegisteredThemes`) and package-based resource
//!   resolution (startup-ui.ts:44-75) are not ported: the theme list comes
//!   from `get_available_themes` (built-ins + global custom themes) instead
//!   of resolved packages.
//! - The terminal used by the dialog is injectable for tests (the
//!   `with_terminal` variant); production uses a `ProcessTerminal`.
//! - Upstream 0.82.1 has no analytics sender: `enableAnalytics` (opt-in,
//!   default false) and the generated `trackingId` are persisted only, so
//!   there is nothing to send here (verified against the pinned tree —
//!   `getEnableAnalytics` has no consumer outside settings/first-time
//!   setup). The install ping is `core::telemetry` (T14-W6a).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_tui::terminal::ProcessTerminal;
use rpi_tui::terminal_colors::TerminalColorScheme;
use rpi_tui::tui::{shared_component_from_boxed, Component, Focusable, Tui};

use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::themes::{get_available_themes, load_theme};
use crate::error::RpiError;

use super::components::extension_selector::ExtensionSelectorComponent;
use super::components::first_time_setup::{FirstTimeSetupComponent, FirstTimeSetupResult};
use super::theme_watcher::detect_terminal_theme_for_auto;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `shouldRunFirstTimeSetup` (startup-ui.ts:115-132): the setup runs when
/// this is an official distribution (always true for the rpi binary), the
/// experimental flag is set, no agent-dir env override is in effect, and the
/// global `settings.json` does not exist yet. Mirrors upstream's
/// `settingsPath = getSettingsPath()` default parameter.
pub(crate) fn should_run_first_time_setup(settings_path: &std::path::Path) -> bool {
    if std::env::var_os(crate::config::ENV_AGENT_DIR).is_some() {
        return false;
    }
    if !crate::core::environment::experimental_enabled() {
        return false;
    }
    !settings_path.exists()
}

/// TUI entry wrapper for the setup dialog: forwards component calls and
/// focus to the shared component; the setup driver loop keeps the concrete
/// `Arc<Mutex<FirstTimeSetupComponent>>` for theme previews.
struct SetupRegion(Arc<Mutex<FirstTimeSetupComponent>>);

impl Component for SetupRegion {
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

impl Focusable for SetupRegion {
    fn focused(&self) -> bool {
        lock(&self.0).focused()
    }

    fn set_focused(&mut self, focused: bool) {
        lock(&self.0).set_focused(focused);
    }
}

/// `showFirstTimeSetup` (startup-ui.ts:166-205) on the process terminal.
pub(crate) async fn run_first_time_setup(runtime: &AgentSessionRuntime) -> Result<bool, RpiError> {
    run_first_time_setup_with_terminal(runtime, Box::new(ProcessTerminal::new())).await
}

/// `showFirstTimeSetup` (startup-ui.ts:166-205) on an injected terminal
/// (test injection point). Returns `Ok(true)` when the user completed the
/// setup (settings persisted), `Ok(false)` when it was skipped or cancelled.
pub(crate) async fn run_first_time_setup_with_terminal(
    runtime: &AgentSessionRuntime,
    terminal: Box<dyn rpi_tui::terminal::Terminal + Send>,
) -> Result<bool, RpiError> {
    // The dialog's input matching reads the global keybinding table
    // (startup-ui.ts:81 `setKeybindings`); reinstall so the setup works even
    // when it is invoked outside `run_interactive_mode` (tests).
    super::interactive_mode::install_global_keybindings();

    let session = runtime.session();
    let ui = Tui::with_options(
        terminal,
        Some(session.settings_manager(|settings| settings.get_show_hardware_cursor())),
        Some(runtime.services().agent_dir.clone()),
    );
    ui.set_clear_on_shrink(session.settings_manager(|settings| settings.get_clear_on_shrink()));

    // Start the TUI and its driver before the terminal queries: the OSC 11 /
    // DSR replies are delivered by `pump` (startup-ui.ts starts the TUI
    // before detecting, lines 186-188).
    ui.start();
    let stop = Arc::new(AtomicBool::new(false));
    let driver_ui = ui.clone();
    let driver_stop = Arc::clone(&stop);
    let driver = std::thread::Builder::new()
        .name("rpi-setup-driver".to_string())
        .spawn(move || {
            while !driver_stop.load(Ordering::Relaxed) {
                driver_ui.pump(Some(Duration::from_millis(50)));
            }
        })?;

    let detected = detect_terminal_theme_for_auto(&ui, 100).await;
    let detected_name = match detected {
        TerminalColorScheme::Dark => "dark",
        TerminalColorScheme::Light => "light",
    };

    let (submit_tx, submit_rx) = tokio::sync::oneshot::channel();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let mut submit_tx = Some(submit_tx);
    let mut cancel_tx = Some(cancel_tx);
    let component = Arc::new(Mutex::new(FirstTimeSetupComponent::new(
        Arc::new(load_theme(detected_name, None)?),
        get_available_themes()
            .into_iter()
            .map(|info| info.name)
            .collect(),
        Some(detected_name.to_string()),
        Box::new(move |result: FirstTimeSetupResult| {
            if let Some(tx) = submit_tx.take() {
                let _ = tx.send(Some(result));
            }
        }),
        Box::new(move || {
            if let Some(tx) = cancel_tx.take() {
                let _ = tx.send(());
            }
        }),
    )));

    // Theme preview (startup-ui.ts:191-194): swap the dialog's render theme
    // and repaint.
    let preview_component = Arc::clone(&component);
    let preview_ui = ui.clone();
    lock(&component).on_theme_preview = Some(Box::new(move |name: &str| {
        if let Ok(theme) = load_theme(name, None) {
            lock(&preview_component).set_theme(Arc::new(theme));
        }
        preview_ui.invalidate();
        preview_ui.request_render(false);
    }));

    let entry = shared_component_from_boxed(Box::new(SetupRegion(Arc::clone(&component))));
    ui.add_child(entry.clone());
    ui.set_focus(Some(entry));
    ui.request_render(false);

    let completed = tokio::select! {
        result = submit_rx => result.ok().flatten(),
        _ = cancel_rx => None,
    };

    stop.store(true, Ordering::Relaxed);
    let _ = driver.join();
    ui.stop();

    if let Some(result) = completed {
        // Persist the result (startup-ui.ts:176-179).
        session.settings_manager(|settings| {
            settings.set_theme(&result.theme);
            settings.set_enable_analytics(result.analytics);
        });
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Startup selector (project-trust prompt)
// ---------------------------------------------------------------------------

/// TUI entry wrapper for the startup selector (same shape as
/// [`SetupRegion`]): forwards component calls and focus.
struct SelectorRegion(Arc<Mutex<ExtensionSelectorComponent>>);

impl Component for SelectorRegion {
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

impl Focusable for SelectorRegion {
    fn focused(&self) -> bool {
        // Always ready for input while shown (the setup wrapper pattern).
        true
    }

    fn set_focused(&mut self, _focused: bool) {}
}

/// `showStartupSelector` (startup-ui.ts:134-164): a selector shown before
/// the interactive TUI exists, on its own short-lived `Tui`. Blocks until
/// the user confirms an option (its label) or cancels (`None`).
///
/// Intentional differences:
/// - Synchronous: the trust store callers are sync (trust-manager.ts:156-159);
///   the pump driver thread runs while the caller blocks on the result
///   channel.
/// - Package-based startup themes (`loadStartupThemes`, startup-ui.ts:60-75)
///   are not ported — same exemption as the first-time setup dialog.
/// - `clearStartupTui`'s 25ms repaint delay is dropped; `ui.stop()` restores
///   the terminal.
pub(crate) fn run_startup_selector(
    settings: &crate::core::settings_manager::SettingsManager,
    title: &str,
    options: &[String],
) -> Option<String> {
    run_startup_selector_with_terminal(settings, title, options, Box::new(ProcessTerminal::new()))
}

/// Terminal-injectable variant (test seam).
pub(crate) fn run_startup_selector_with_terminal(
    settings: &crate::core::settings_manager::SettingsManager,
    title: &str,
    options: &[String],
    terminal: Box<dyn rpi_tui::terminal::Terminal + Send>,
) -> Option<String> {
    super::interactive_mode::install_global_keybindings();

    // `createStartupTui` theme resolution (startup-ui.ts:78-87), minus
    // package themes and the async OSC re-detection.
    let terminal_theme = crate::core::themes::detect_terminal_background_from_env().theme;
    let theme_name =
        crate::core::themes::resolve_theme_setting(settings.get_theme().as_deref(), terminal_theme)
            .unwrap_or_else(|| terminal_theme.as_str().to_string());
    let theme = match load_theme(&theme_name, None) {
        Ok(theme) => Arc::new(theme),
        Err(_) => return None,
    };

    let ui = Tui::with_options(
        terminal,
        Some(settings.get_show_hardware_cursor()),
        Some(crate::config::get_agent_dir()),
    );
    ui.set_clear_on_shrink(settings.get_clear_on_shrink());
    ui.start();
    let stop = Arc::new(AtomicBool::new(false));
    let driver_ui = ui.clone();
    let driver_stop = Arc::clone(&stop);
    let driver = std::thread::Builder::new()
        .name("rpi-startup-selector".to_string())
        .spawn(move || {
            while !driver_stop.load(Ordering::Relaxed) {
                driver_ui.pump(Some(Duration::from_millis(50)));
            }
        });

    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let fire = {
        let tx = Arc::clone(&tx);
        move |value: Option<String>| {
            if let Some(tx) = lock(&tx).take() {
                let _ = tx.send(value);
            }
        }
    };
    let on_select = {
        let fire = fire;
        Box::new(move |value: Option<String>| fire(value)) as Box<dyn FnMut(Option<String>) + Send>
    };
    let on_cancel = Box::new(move || {}) as Box<dyn FnMut() + Send>;
    let component = Arc::new(Mutex::new(ExtensionSelectorComponent::new(
        theme,
        Some(title.to_string()),
        options.to_vec(),
        on_select,
        on_cancel,
        None,
    )));

    let Ok(driver) = driver else {
        // No pump thread → no input; fail closed (cancel).
        ui.stop();
        return None;
    };

    let entry = shared_component_from_boxed(Box::new(SelectorRegion(Arc::clone(&component))));
    ui.add_child(entry.clone());
    ui.set_focus(Some(entry));
    ui.request_render(false);

    // Block until the component fires (confirm/cancel); a disconnected
    // channel means the UI died — treat as cancel.
    let result = rx.recv().unwrap_or(None);

    stop.store(true, Ordering::Relaxed);
    let _ = driver.join();
    ui.stop();
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc as StdArc;

    use rpi_tui::terminal::Terminal;

    use super::*;
    use crate::modes::interactive::test_support::{build_test_session, TestTerminal};

    /// RAII guard restoring an env var on drop.
    struct EnvGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let original = std::env::var_os(name);
            std::env::set_var(name, value);
            EnvGuard { name, original }
        }

        fn remove(name: &'static str) -> Self {
            let original = std::env::var_os(name);
            std::env::remove_var(name);
            EnvGuard { name, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[test]
    fn should_run_first_time_setup_checks_settings_file_and_env() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        // Experimental flag on, no agent-dir override, no settings file.
        let _experimental = EnvGuard::set(crate::core::environment::ENV_EXPERIMENTAL, "1");
        let _agent_dir = EnvGuard::remove(crate::config::ENV_AGENT_DIR);

        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        let settings_path = agent_dir.join("settings.json");
        assert!(should_run_first_time_setup(&settings_path));

        // Existing settings file marks the setup as done (settings.json is
        // the upstream first-run marker, startup-ui.ts:131).
        std::fs::write(&settings_path, "{}").expect("write settings");
        assert!(!should_run_first_time_setup(&settings_path));

        // Experimental flag off disables the setup.
        let _experimental_off = EnvGuard::set(crate::core::environment::ENV_EXPERIMENTAL, "");
        assert!(!should_run_first_time_setup(&settings_path));
    }

    #[test]
    fn should_run_first_time_setup_respects_agent_dir_override() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        let _experimental = EnvGuard::set(crate::core::environment::ENV_EXPERIMENTAL, "1");
        let _agent_dir = EnvGuard::set(crate::config::ENV_AGENT_DIR, "/tmp/rpi-test-override");

        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let settings_path = tmp.path().join("agent").join("settings.json");
        assert!(!should_run_first_time_setup(&settings_path));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn first_time_setup_submit_persists_theme_and_analytics() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        let _experimental = EnvGuard::set(crate::core::environment::ENV_EXPERIMENTAL, "1");
        let _agent_dir = EnvGuard::remove(crate::config::ENV_AGENT_DIR);

        let harness = build_test_session().await;
        let terminal = StdArc::new(TestTerminal::new());
        let terminal_box: Box<dyn Terminal + Send> = Box::new(TestTerminal::clone(&terminal));

        let setup = tokio::spawn(async move {
            run_first_time_setup_with_terminal(&harness.runtime, terminal_box).await
        });

        // Wait for the TUI to start and the detection timeout to elapse
        // (the test terminal never replies to OSC/DSR queries, so detection
        // resolves via the Tui's query deadline after ~100ms), then walk the
        // two-step dialog: theme → analytics → submit.
        while !terminal.is_started() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        terminal.feed("\r");
        terminal.feed("\r");

        let completed = setup.await.expect("setup task").expect("no setup error");
        assert!(completed, "submitted setup reports completion");

        let (theme, analytics) = harness
            .session
            .settings_manager(|settings| (settings.get_theme(), settings.get_enable_analytics()));
        // The submitted theme follows the detected terminal appearance
        // (built-in themes only; the harness terminal can't answer queries,
        // so it's the env/fallback branch — either built-in is valid).
        assert!(
            matches!(theme.as_deref(), Some("dark" | "light")),
            "theme persisted, got {theme:?}"
        );
        assert!(analytics, "analytics opt-in persisted (default selection)");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn first_time_setup_cancel_skips_persisting() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        let _experimental = EnvGuard::set(crate::core::environment::ENV_EXPERIMENTAL, "1");
        let _agent_dir = EnvGuard::remove(crate::config::ENV_AGENT_DIR);

        let harness = build_test_session().await;
        let terminal = StdArc::new(TestTerminal::new());
        let terminal_box: Box<dyn Terminal + Send> = Box::new(TestTerminal::clone(&terminal));

        let setup = tokio::spawn(async move {
            run_first_time_setup_with_terminal(&harness.runtime, terminal_box).await
        });

        while !terminal.is_started() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        terminal.feed("\x1b"); // escape skips the setup

        let completed = setup.await.expect("setup task").expect("no setup error");
        assert!(!completed, "cancelled setup reports no completion");

        let theme = harness
            .session
            .settings_manager(|settings| settings.get_theme());
        assert_eq!(theme, None, "no theme persisted on cancel");
    }

    #[test]
    fn startup_selector_returns_selected_label() {
        let settings = crate::core::settings_manager::SettingsManager::in_memory(
            crate::core::settings_manager::Settings::new(),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let terminal = StdArc::new(TestTerminal::new());
        let terminal_box: Box<dyn Terminal + Send> = Box::new(TestTerminal::clone(&terminal));
        let options = vec!["Trust".to_string(), "Do not trust".to_string()];

        let selector = std::thread::spawn(move || {
            run_startup_selector_with_terminal(
                &settings,
                "Trust project folder?",
                &options,
                terminal_box,
            )
        });

        while !terminal.is_started() {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let the focus settle before feeding input.
        std::thread::sleep(Duration::from_millis(100));
        // Down + confirm selects the second option.
        terminal.feed("\x1b[B");
        terminal.feed("\r");
        let selected = selector.join().expect("selector thread");
        assert_eq!(selected.as_deref(), Some("Do not trust"));
    }

    #[test]
    fn startup_selector_escape_cancels() {
        let settings = crate::core::settings_manager::SettingsManager::in_memory(
            crate::core::settings_manager::Settings::new(),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let terminal = StdArc::new(TestTerminal::new());
        let terminal_box: Box<dyn Terminal + Send> = Box::new(TestTerminal::clone(&terminal));

        let selector = std::thread::spawn(move || {
            run_startup_selector_with_terminal(&settings, "t", &["a".to_string()], terminal_box)
        });

        while !terminal.is_started() {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        terminal.feed("\x1b");
        let selected = selector.join().expect("selector thread");
        assert_eq!(selected, None);
    }
}
