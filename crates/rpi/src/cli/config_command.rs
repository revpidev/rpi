//! Port of `handleConfigCommand` (`packages/coding-agent/src/
//! package-manager-cli.ts:603-674`) and `selectConfig`
//! (`packages/coding-agent/src/cli/config-selector.ts`) @ pi 0.82.1
//! (2efa728) — the `rpi config` resource-configuration TUI (T14-W3).
//!
//! Intentional differences (D-042):
//! - Project trust resolves headless through
//!   `trust_manager::resolve_project_trusted` (no UI prompt, no
//!   `project_trust` extension event — W4/T15); `-a`/`-na` override.
//! - Upstream ends the command with `process.exit(0)`; here the TUI close
//!   and ctrl+c exit paths both return exit code 0 to the caller.
//! - The resolved resource lists come from the package manager's full
//!   resolve ([`DefaultPackageManager::resolve_all`]); the selector writes
//!   toggles straight into the shared settings manager.
//! - The theme watcher is not started (same precedent as the session
//!   picker); the terminal is injectable for tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_tui::terminal::ProcessTerminal;
use rpi_tui::tui::{shared_component_from_boxed, Component, Focusable, TuiStopOptions};
use rpi_tui::tui_main_screen::TuiMainScreen;

use crate::config::{APP_NAME, CONFIG_DIR_NAME};
use crate::core::package_manager::{DefaultPackageManager, PackageCommandRunner, ResolvedPaths};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::themes::load_theme;
use crate::core::trust_manager::{
    default_project_trust_from_settings, resolve_project_trusted, ProjectTrustContext,
    ProjectTrustStore,
};
use crate::error::RpiError;
use crate::modes::interactive::components::config_selector::{
    ConfigSelectorComponent, ConfigWriteScope, ScopedResolvedPaths,
};

/// `CONFIG_COMMAND_USAGE` (package-manager-cli.ts:92).
pub const CONFIG_USAGE: &str = "rpi config [-l] [--approve|--no-approve]";

/// `printConfigCommandHelp` (package-manager-cli.ts:94-107), plain text
/// (headless stdout; upstream chalk bold is TTY-only).
pub fn config_help() -> String {
    format!(
        r#"Usage:
  {CONFIG_USAGE}

Open the resource configuration TUI to enable or disable package resources.
Without -l, starts in global settings (~/{CONFIG_DIR_NAME}/agent/settings.json).
Press Tab in the TUI to switch between global and project-local modes.

Options:
  -l, --local       Edit project overrides ({CONFIG_DIR_NAME}/settings.json)
  -a, --approve     Trust project-local files for this command with -l
  -na, --no-approve Ignore project-local files for this command with -l
"#
    )
}

/// Parsed `config` command (the `handleConfigCommand` argument loop,
/// package-manager-cli.ts:617-637).
#[derive(Debug, Default)]
pub struct ParsedConfig {
    pub local: bool,
    pub project_trust_override: Option<bool>,
    pub help: bool,
    pub invalid_option: Option<String>,
    pub invalid_argument: Option<String>,
}

/// The `handleConfigCommand` argument loop. `args[0]` must be `"config"`.
pub fn parse_config_args(args: &[String]) -> ParsedConfig {
    let mut parsed = ParsedConfig::default();
    for arg in &args[1..] {
        if arg == "-h" || arg == "--help" {
            parsed.help = true;
        } else if arg == "-l" || arg == "--local" {
            parsed.local = true;
        } else if arg == "-a" || arg == "--approve" {
            parsed.project_trust_override = Some(true);
        } else if arg == "-na" || arg == "--no-approve" {
            parsed.project_trust_override = Some(false);
        } else if arg.starts_with('-') {
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else {
            parsed.invalid_argument.get_or_insert_with(|| arg.clone());
        }
    }
    parsed
}

/// Everything `handleConfigCommand` computes before the TUI starts —
/// extracted so tests can drive it without a terminal.
struct ConfigPrelude {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings_manager: Arc<Mutex<SettingsManager>>,
    resolved_paths: ScopedResolvedPaths,
    write_scope: ConfigWriteScope,
    project_mode_available: bool,
}

/// Parse + trust + gates + resolve (package-manager-cli.ts:607-662).
/// Returns the process exit code on failure.
fn prepare_config(
    args: &[String],
    cwd: &Path,
    agent_dir: &Path,
    runner: Option<Arc<dyn PackageCommandRunner>>,
) -> Result<ConfigPrelude, i32> {
    let parsed = parse_config_args(args);
    if parsed.help {
        print!("{}", config_help());
        return Err(0);
    }
    if let Some(option) = &parsed.invalid_option {
        eprintln!("Unknown option {option} for \"config\".");
        eprintln!("Use \"{APP_NAME} --help\" or \"{CONFIG_USAGE}\".");
        return Err(1);
    }
    if let Some(argument) = &parsed.invalid_argument {
        eprintln!("Unexpected argument {argument}.");
        eprintln!("Usage: {CONFIG_USAGE}");
        return Err(1);
    }

    // `createCommandSettingsManager` without `useSavedProjectTrustOnly`
    // (package-manager-cli.ts:555-601), headless: no trust extensions and
    // no UI prompt, so trust comes from the override, trust.json, or
    // `defaultProjectTrust` (ask → false).
    let mut settings_manager = SettingsManager::create(
        cwd,
        Some(agent_dir),
        SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    let trust_store = ProjectTrustStore::new(agent_dir);
    let default_project_trust =
        default_project_trust_from_settings(settings_manager.get_default_project_trust());
    let project_trusted = match resolve_project_trusted(
        cwd,
        &trust_store,
        parsed.project_trust_override,
        default_project_trust,
        None,
        &mut ProjectTrustContext::headless(),
    ) {
        Ok(trusted) => trusted,
        Err(error) => {
            eprintln!("Error: {error}");
            return Err(1);
        }
    };
    settings_manager.set_project_trusted(project_trusted);

    if parsed.local && !settings_manager.is_project_trusted() {
        eprintln!("Project is not trusted. Use --approve to modify local resource config.");
        return Err(1);
    }

    // `reportSettingsErrors` (package-manager-cli.ts:69-77, 653).
    for error in settings_manager.drain_errors() {
        eprintln!(
            "Warning (config command, {} settings): {}",
            error.scope.as_str(),
            error.error
        );
    }

    // package-manager-cli.ts:654-662: the global view resolves with a
    // project-untrusted manager; the project view reuses the trusted one.
    let global_settings_manager = SettingsManager::create(
        cwd,
        Some(agent_dir),
        SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    let resolve = |manager: SettingsManager| -> Result<ResolvedPaths, String> {
        DefaultPackageManager::with_options(crate::core::package_manager::PackageManagerOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            settings_manager: manager,
            runner: runner.clone(),
            offline: None,
        })
        .resolve_all(None)
    };
    let global_resolved = match resolve(global_settings_manager) {
        Ok(paths) => paths,
        Err(message) => {
            eprintln!("Error: {message}");
            return Err(1);
        }
    };
    let project_resolved = if settings_manager.is_project_trusted() {
        // Upstream reuses the command's settings manager
        // (package-manager-cli.ts:660-662); `resolve` only reads it, so a
        // fresh trusted manager over the same files is equivalent.
        let project_manager = SettingsManager::create(
            cwd,
            Some(agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: true,
            },
        );
        match resolve(project_manager) {
            Ok(paths) => paths,
            Err(message) => {
                eprintln!("Error: {message}");
                return Err(1);
            }
        }
    } else {
        global_resolved.clone()
    };

    let project_mode_available = settings_manager.is_project_trusted();
    Ok(ConfigPrelude {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        settings_manager: Arc::new(Mutex::new(settings_manager)),
        resolved_paths: ScopedResolvedPaths {
            global: global_resolved,
            project: project_resolved,
        },
        write_scope: if parsed.local {
            ConfigWriteScope::Project
        } else {
            ConfigWriteScope::Global
        },
        project_mode_available,
    })
}

/// TUI entry wrapper: forwards component calls and focus to the shared
/// component (same idiom as the session picker's `PickerRegion`).
struct ConfigRegion(Arc<Mutex<ConfigSelectorComponent>>);

impl Component for ConfigRegion {
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

impl Focusable for ConfigRegion {
    fn focused(&self) -> bool {
        lock(&self.0).focused()
    }

    fn set_focused(&mut self, focused: bool) {
        lock(&self.0).set_focused(focused);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `handleConfigCommand` (package-manager-cli.ts:603-674). `args[0]` must
/// be `"config"`. Returns the process exit code.
pub async fn run_config(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let agent_dir = crate::config::get_agent_dir();
    run_config_with_terminal(
        args,
        &cwd,
        &agent_dir,
        None,
        Box::new(ProcessTerminal::new()),
    )
    .await
}

/// [`run_config`] on explicit directories, an injectable command runner,
/// and an injectable terminal (tests drive the TUI through a
/// `TestTerminal`; no real `~/.rpi`, process, or network is touched).
pub async fn run_config_with_terminal(
    args: &[String],
    cwd: &Path,
    agent_dir: &Path,
    runner: Option<Arc<dyn PackageCommandRunner>>,
    terminal: Box<dyn rpi_tui::terminal::Terminal + Send>,
) -> i32 {
    let prelude = match prepare_config(args, cwd, agent_dir, runner) {
        Ok(prelude) => prelude,
        Err(code) => return code,
    };
    match select_config(prelude, terminal).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// `selectConfig` (cli/config-selector.ts:20-56): run the selector TUI
/// until it closes. Both the close and the ctrl+c exit path map to the
/// upstream `process.exit(0)`.
async fn select_config(
    prelude: ConfigPrelude,
    terminal: Box<dyn rpi_tui::terminal::Terminal + Send>,
) -> Result<(), RpiError> {
    // The component's input matching reads the global keybinding tables.
    crate::modes::interactive::interactive_mode::install_global_keybindings();

    // `initTheme` (cli/config-selector.ts:22): theme from settings, dark
    // fallback (session-picker precedent); the watcher stays with the
    // interactive mode.
    let (theme_name, show_hardware_cursor, clear_on_shrink) = {
        let manager = lock(&prelude.settings_manager);
        (
            manager.get_theme().unwrap_or_else(|| "dark".to_string()),
            manager.get_show_hardware_cursor(),
            manager.get_clear_on_shrink(),
        )
    };
    let theme = load_theme(&theme_name, None)
        .or_else(|_| load_theme("dark", None))
        .map_err(|error| RpiError::Resource(error.to_string()))?;
    let ui = TuiMainScreen::with_options(
        terminal,
        Some(show_hardware_cursor),
        Some(prelude.agent_dir.clone()),
    );
    ui.set_clear_on_shrink(clear_on_shrink);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));
    let finish = |done_tx: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>| {
        if let Some(tx) = lock(done_tx).take() {
            let _ = tx.send(());
        }
    };
    let on_cancel = {
        let done_tx = Arc::clone(&done_tx);
        Box::new(move || finish(&done_tx)) as Box<dyn FnMut() + Send>
    };
    let on_exit = {
        let done_tx = Arc::clone(&done_tx);
        Box::new(move || finish(&done_tx)) as Box<dyn FnMut() + Send>
    };
    let selector = Arc::new(Mutex::new(ConfigSelectorComponent::new(
        prelude.resolved_paths,
        prelude.settings_manager,
        Arc::new(theme),
        &prelude.cwd.to_string_lossy(),
        &prelude.agent_dir.to_string_lossy(),
        Some(usize::from(ui.terminal_rows())),
        prelude.write_scope,
        prelude.project_mode_available,
        Some(on_cancel),
        Some(on_exit),
    )));

    let entry = shared_component_from_boxed(Box::new(ConfigRegion(selector)));
    ui.add_child(entry.clone());
    ui.set_focus(Some(entry));
    ui.request_render(false);

    // Start LAST (session-picker precedent) so no input can arrive before
    // the selector is focused.
    ui.start();
    let stop = Arc::new(AtomicBool::new(false));
    let driver_ui = ui.clone();
    let driver_stop = Arc::clone(&stop);
    let driver = std::thread::Builder::new()
        .name("rpi-config-driver".to_string())
        .spawn(move || {
            while !driver_stop.load(Ordering::Relaxed) {
                driver_ui.pump(Some(Duration::from_millis(50)));
            }
        })
        .map_err(RpiError::Io)?;

    let _ = done_rx.await;

    stop.store(true, Ordering::Relaxed);
    let _ = driver.join();
    ui.stop(TuiStopOptions::default());

    Ok(())
}

#[cfg(test)]
mod tests {
    //! The pre-TUI half of `handleConfigCommand`: parsing, trust gates, and
    //! the resolved-paths split; plus a TestTerminal drive of the TUI.

    use super::*;
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirs {
        root: PathBuf,
        cwd: PathBuf,
        agent_dir: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let unique = format!(
                "rpi-config-cmd-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            let cwd = root.join("cwd");
            let agent_dir = root.join("agent");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&agent_dir).unwrap();
            TestDirs {
                root,
                cwd,
                agent_dir,
            }
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn args(input: &[&str]) -> Vec<String> {
        input.iter().map(|s| s.to_string()).collect()
    }

    // ---- parse (package-manager-cli.ts:617-637) ----

    #[test]
    fn test_parse_config_flags() {
        let parsed = parse_config_args(&args(&["config"]));
        assert!(!parsed.local);
        assert_eq!(parsed.project_trust_override, None);

        let parsed = parse_config_args(&args(&["config", "-l", "-a"]));
        assert!(parsed.local);
        assert_eq!(parsed.project_trust_override, Some(true));

        let parsed = parse_config_args(&args(&["config", "--local", "--no-approve"]));
        assert!(parsed.local);
        assert_eq!(parsed.project_trust_override, Some(false));

        let parsed = parse_config_args(&args(&["config", "-na"]));
        assert_eq!(parsed.project_trust_override, Some(false));
    }

    #[test]
    fn test_parse_config_rejects_unknown_options_and_positionals() {
        let parsed = parse_config_args(&args(&["config", "--bogus"]));
        assert_eq!(parsed.invalid_option.as_deref(), Some("--bogus"));
        let parsed = parse_config_args(&args(&["config", "foo"]));
        assert_eq!(parsed.invalid_argument.as_deref(), Some("foo"));
        let parsed = parse_config_args(&args(&["config", "--help"]));
        assert!(parsed.help);
    }

    #[test]
    fn test_help_text() {
        let help = config_help();
        assert!(help.contains(CONFIG_USAGE));
        assert!(help.contains("-l, --local"));
        assert!(help.contains("Tab"));
    }

    // ---- trust gates (package-manager-cli.ts:648-652) ----

    #[test]
    fn test_local_requires_project_trust() {
        // A pre-existing `.rpi/settings.json` requires trust; the headless
        // default (ask → false) blocks `-l`.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join(".rpi")).unwrap();
        std::fs::write(dirs.cwd.join(".rpi/settings.json"), "{}").unwrap();
        let code = prepare_config(&args(&["config", "-l"]), &dirs.cwd, &dirs.agent_dir, None)
            .err()
            .expect("blocked");
        assert_eq!(code, 1);

        // `-a` approves for this command.
        let prelude = prepare_config(
            &args(&["config", "-l", "-a"]),
            &dirs.cwd,
            &dirs.agent_dir,
            None,
        );
        assert!(prelude.is_ok());
    }

    #[test]
    fn test_local_allowed_without_trust_requiring_resources() {
        let dirs = TestDirs::new();
        let prelude = prepare_config(&args(&["config", "-l"]), &dirs.cwd, &dirs.agent_dir, None)
            .expect("allowed");
        assert_eq!(prelude.write_scope, ConfigWriteScope::Project);
        assert!(prelude.project_mode_available);
    }

    #[test]
    fn test_invalid_option_exits_1_before_any_io() {
        let dirs = TestDirs::new();
        let code = prepare_config(
            &args(&["config", "--bogus"]),
            &dirs.cwd,
            &dirs.agent_dir,
            None,
        )
        .err()
        .expect("invalid");
        assert_eq!(code, 1);
    }

    #[test]
    fn test_untrusted_project_view_falls_back_to_global() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join(".rpi")).unwrap();
        std::fs::write(dirs.cwd.join(".rpi/settings.json"), "{}").unwrap();
        let prelude =
            prepare_config(&args(&["config"]), &dirs.cwd, &dirs.agent_dir, None).expect("prelude");
        assert!(!prelude.project_mode_available);
        assert_eq!(prelude.write_scope, ConfigWriteScope::Global);
    }

    // ---- TUI drive (selectConfig) ----

    #[tokio::test]
    async fn test_tui_escape_closes_with_exit_0() {
        let dirs = TestDirs::new();
        let terminal = crate::modes::interactive::test_support::TestTerminal::new();
        let config_args = args(&["config"]);
        let driver = run_config_with_terminal(
            &config_args,
            &dirs.cwd,
            &dirs.agent_dir,
            None,
            Box::new(terminal.clone()),
        );
        let feeder = async {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !terminal.is_started() {
                assert!(std::time::Instant::now() < deadline, "TUI never started");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            terminal.feed("\x1b");
        };
        let (code, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(driver, feeder)
        })
        .await
        .expect("config TUI timed out");
        assert_eq!(code, 0);
    }
}
