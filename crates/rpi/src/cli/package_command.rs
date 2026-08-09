//! Port of the package-command branches of `packages/coding-agent/src/
//! package-manager-cli.ts` (`parsePackageCommand` + `handlePackageCommand`)
//! @ pi 0.82.1 (2efa728) — W6-C landed the `update --models` target (remote
//! model catalog refresh); T14-W2 landed `install` / `remove`
//! (`uninstall`) / `list`; T14-W3 lands the remaining `update` targets
//! (self / extensions / all / single-source) with the self-update plan,
//! release-note rendering, and install-method command construction. The
//! `config` command lives in `cli::config_command`.
//!
//! Intentional differences (D-037, D-041):
//! - `--force` is inert for the models target (the refresh always runs with
//!   `force: true`, upstream `refreshModelCatalogs`).
//! - Project trust on the update path resolves through the stored
//!   trust.json decision only (`useSavedProjectTrustOnly: true`,
//!   package-manager-cli.ts:565-569): no UI prompt, no `project_trust`
//!   extension event (W4/T15); `-a`/`-na` override.
//! - Output is plain text (headless stdout); upstream chalk styling is
//!   TTY-only. The release note renders through the rpi-tui Markdown
//!   component with the identity theme.

use std::time::Duration;

use rpi_ai::models::ModelsRefreshOptions;
use rpi_tui::components::markdown::{Markdown, MarkdownTheme};
use rpi_tui::tui::Component;
use tokio_util::sync::CancellationToken;

use crate::config::{
    detect_install_method, get_self_update_command, get_self_update_unavailable_instruction,
    system_command_probe, InstallMethod, SelfUpdateCommand, SelfUpdatePackageTarget, APP_NAME,
    PACKAGE_NAME, VERSION,
};
use crate::core::model_runtime::{
    CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput, DEFAULT_MODEL_REFRESH_TIMEOUT_MS,
};
use crate::core::package_manager::{
    CommandRequest, ConfiguredPackage, DefaultPackageManager, PackageCommandRunner,
    SystemPackageCommandRunner,
};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::skills::SourceScope;
use crate::core::trust_manager::{
    default_project_trust_from_settings, resolve_project_trusted, ProjectTrustContext,
    ProjectTrustStore,
};
use crate::core::version_check::{
    get_latest_rpi_release_with, is_newer_package_version, version_check_endpoint,
    LatestVersionTransport, ReqwestLatestVersionTransport, DEFAULT_VERSION_CHECK_TIMEOUT,
};
use std::path::Path;
use std::sync::Arc;

/// `getPackageCommandUsage("update")` (package-manager-cli.ts:86). The
/// literal `rpi` mirrors `APP_NAME` (config.rs); the
/// `usage_lines_start_with_app_name` test binds the two so a rename cannot
/// silently leave the help text stale (T14 review N-1).
pub const UPDATE_USAGE: &str =
    "rpi update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]";

/// `UpdateTarget` (package-manager-cli.ts:35).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateTarget {
    All,
    Self_,
    Extensions { source: Option<String> },
    Models,
}

/// Parsed `update` command (the `PackageCommandOptions` subset the update
/// branch produces, package-manager-cli.ts:189-387).
#[derive(Debug, Default)]
pub struct ParsedUpdate {
    pub help: bool,
    pub invalid_option: Option<String>,
    pub invalid_argument: Option<String>,
    pub missing_option_value: Option<String>,
    pub conflicting_options: Option<String>,
    pub target: Option<UpdateTarget>,
    /// `--force` — reinstall rpi even when the current version is latest
    /// (self target only; inert for the models target, see module docs).
    pub force: bool,
    /// `-a`/`--approve` / `-na`/`--no-approve`.
    pub project_trust_override: Option<bool>,
    /// Bare `rpi update` prints the extensions-skipped note
    /// (package-manager-cli.ts:368-369).
    pub show_extensions_skipped_note: bool,
}

/// `parsePackageCommand` restricted to `update` (package-manager-cli.ts:
/// 216-371). Non-update package commands are not handled here.
pub fn parse_update_args(args: &[String]) -> ParsedUpdate {
    let rest = &args[1..];
    let mut parsed = ParsedUpdate::default();
    let mut source: Option<String> = None;
    let mut self_flag = false;
    let mut extensions_flag = false;
    let mut models_flag = false;
    let mut all_flag = false;
    let mut extension_flag_source: Option<String> = None;

    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "-h" || arg == "--help" {
            parsed.help = true;
        } else if arg == "-l" || arg == "--local" {
            // Valid for install/remove only (package-manager-cli.ts:223-230).
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if arg == "--self" {
            self_flag = true;
        } else if arg == "--extensions" {
            extensions_flag = true;
        } else if arg == "--models" {
            models_flag = true;
        } else if arg == "--all" {
            all_flag = true;
        } else if arg == "--approve" || arg == "-a" {
            parsed.project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            parsed.project_trust_override = Some(false);
        } else if arg == "--force" {
            // Only consumed by the self target (the models refresh always
            // forces; see module docs).
            parsed.force = true;
        } else if arg == "--extension" {
            let value = rest.get(index + 1);
            match value {
                Some(value) if !value.starts_with('-') => {
                    if extension_flag_source.is_some() {
                        parsed.conflicting_options.get_or_insert_with(|| {
                            "--extension can only be provided once".to_owned()
                        });
                    } else {
                        extension_flag_source = Some(value.clone());
                    }
                    index += 1;
                }
                _ => {
                    parsed
                        .missing_option_value
                        .get_or_insert_with(|| arg.clone());
                }
            }
        } else if arg.starts_with('-') {
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if source.is_none() {
            source = Some(arg.clone());
        } else {
            parsed.invalid_argument.get_or_insert_with(|| arg.clone());
        }
        index += 1;
    }

    // Update target resolution + conflicts (package-manager-cli.ts:320-370).
    if all_flag && (self_flag || extensions_flag || models_flag || extension_flag_source.is_some())
    {
        parsed.conflicting_options.get_or_insert_with(|| {
            "--all cannot be combined with --self, --extensions, --models, or --extension"
                .to_owned()
        });
    }
    if all_flag && source.is_some() {
        parsed
            .conflicting_options
            .get_or_insert_with(|| "--all cannot be combined with a positional source".to_owned());
    }

    if models_flag {
        if self_flag || extensions_flag || all_flag || extension_flag_source.is_some() {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--models cannot be combined with --self, --extensions, --all, or --extension"
                    .to_owned()
            });
        }
        if source.is_some() {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--models cannot be combined with a positional source".to_owned()
            });
        }
        parsed.target = Some(UpdateTarget::Models);
    } else if let Some(extension_source) = extension_flag_source {
        if self_flag || extensions_flag || all_flag {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--extension cannot be combined with --self, --extensions, or --all".to_owned()
            });
        }
        if source.is_some() {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--extension cannot be combined with a positional source".to_owned()
            });
        }
        parsed.target = Some(UpdateTarget::Extensions {
            source: Some(extension_source),
        });
    } else if let Some(positional) = source {
        if positional == "self" || positional == "pi" {
            parsed.target = Some(if extensions_flag {
                UpdateTarget::All
            } else {
                UpdateTarget::Self_
            });
        } else {
            if extensions_flag || self_flag || all_flag {
                parsed.conflicting_options.get_or_insert_with(|| {
                    "positional update targets cannot be combined with --self, --extensions, or --all"
                        .to_owned()
                });
            }
            parsed.target = Some(UpdateTarget::Extensions {
                source: Some(positional),
            });
        }
    } else if all_flag || (self_flag && extensions_flag) {
        parsed.target = Some(UpdateTarget::All);
    } else if self_flag {
        parsed.target = Some(UpdateTarget::Self_);
    } else if extensions_flag {
        parsed.target = Some(UpdateTarget::Extensions { source: None });
    } else {
        parsed.target = Some(UpdateTarget::Self_);
        parsed.show_extensions_skipped_note = true;
    }
    parsed
}

/// `printPackageCommandHelp("update")` (package-manager-cli.ts:150-173),
/// plain text (headless stdout; upstream chalk bold is TTY-only).
pub fn update_help() -> String {
    format!(
        r#"Usage:
  {UPDATE_USAGE}

Update pi, installed packages, or model catalogs.

Options:
  --self                  Update pi only (default when no target is given)
  --extensions            Update installed packages only
  --models                Refresh model catalogs only
  --all                   Update pi and installed packages
  --extension <source>    Update one package only
  -a, --approve           Trust project-local files for this command
  -na, --no-approve       Ignore project-local files for this command
  --force                 Reinstall pi even if the current version is latest

Short forms:
  {APP_NAME} update                Update pi only
  {APP_NAME} update --all          Update pi and all extensions
  {APP_NAME} update --models       Refresh model catalogs only
  {APP_NAME} update <source>       Update one package
  {APP_NAME} update pi             Update pi only (self works as alias to pi)
"#
    )
}

/// `refreshModelCatalogs` (package-manager-cli.ts:397-421): create the
/// runtime offline (cache restore only), then force-refresh model catalogs
/// over the network with a 15s timeout.
async fn refresh_model_catalogs() -> Result<(), String> {
    let agent_dir = crate::config::get_agent_dir();
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
        allow_model_network: false,
        ..Default::default()
    })
    .await;
    let token = CancellationToken::new();
    let abort = token.clone();
    let timeout = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(DEFAULT_MODEL_REFRESH_TIMEOUT_MS)).await;
        abort.cancel();
    });
    let result = runtime
        .refresh(Some(ModelsRefreshOptions {
            allow_network: Some(true),
            force: Some(true),
            signal: Some(token),
        }))
        .await;
    timeout.abort();
    if result.aborted {
        return Err("Model catalog refresh timed out.".to_owned());
    }
    if !result.errors.is_empty() {
        let details: Vec<String> = result
            .errors
            .iter()
            .map(|(provider, message)| format!("{provider}: {message}"))
            .collect();
        return Err(format!(
            "Could not refresh model catalogs: {}",
            details.join("; ")
        ));
    }
    Ok(())
}

/// `SelfUpdatePlan` (package-manager-cli.ts:467-473).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdatePlan {
    pub package_name: String,
    pub install_spec: String,
    pub version: String,
    pub should_run: bool,
    pub note: Option<String>,
}

/// `getSelfUpdatePlan` (package-manager-cli.ts:475-501). `endpoint` is the
/// resolved version-check URL (T14-W6a, ADR-0002 §8): `None` disables the
/// probe entirely (no request), surfacing the same error as an
/// unreachable/empty upstream response.
async fn get_self_update_plan(
    force: bool,
    transport: &dyn LatestVersionTransport,
    endpoint: Option<&str>,
) -> Result<SelfUpdatePlan, String> {
    let Some(url) = endpoint else {
        return Err(format!("Could not determine latest {APP_NAME} version."));
    };
    let latest_release = get_latest_rpi_release_with(
        VERSION,
        transport,
        url,
        DEFAULT_VERSION_CHECK_TIMEOUT,
        crate::core::package_manager::is_offline_mode_enabled(),
    )
    .await
    .map_err(|message| format!("Could not determine latest {APP_NAME} version: {message}"))?;
    let Some(latest_release) = latest_release else {
        return Err(format!("Could not determine latest {APP_NAME} version."));
    };

    let package_name = latest_release
        .package_name
        .unwrap_or_else(|| PACKAGE_NAME.to_string());
    let install_spec = format!("{package_name}@{}", latest_release.version);
    if force
        || package_name != PACKAGE_NAME
        || is_newer_package_version(&latest_release.version, VERSION)
    {
        return Ok(SelfUpdatePlan {
            package_name,
            install_spec,
            version: latest_release.version,
            should_run: true,
            note: latest_release.note,
        });
    }

    println!("{APP_NAME} is already up to date (v{VERSION})");
    Ok(SelfUpdatePlan {
        package_name,
        install_spec,
        version: latest_release.version,
        should_run: false,
        note: None,
    })
}

/// `runSelfUpdate` (package-manager-cli.ts:503-524): the steps run with
/// inherited stdio through the package command runner.
fn run_self_update(
    command: &SelfUpdateCommand,
    runner: &dyn PackageCommandRunner,
) -> Result<(), String> {
    println!("Updating {APP_NAME} with {}...", command.display);
    for step in &command.steps {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        runner.run(&CommandRequest::new(&step.command, &args))?;
    }
    Ok(())
}

/// `printSelfUpdateUnavailable` (package-manager-cli.ts:424-436). The
/// executable location line uses the rpi name (ADR-0001); upstream's
/// literal is `Location of pi executable: …` with `process.argv[1]`, and
/// the current executable takes the argv[1] role here (D-041).
fn print_self_update_unavailable(
    npm_command: Option<&[String]>,
    update_package_target: &SelfUpdatePackageTarget,
) {
    eprintln!("error: {APP_NAME} cannot self-update this installation.");
    eprintln!(
        "{}",
        get_self_update_unavailable_instruction(
            PACKAGE_NAME,
            npm_command,
            update_package_target,
            &system_command_probe,
        )
    );
    if let Ok(exe) = std::env::current_exe() {
        eprintln!();
        eprintln!("Location of {APP_NAME} executable: {}", exe.display());
    }
}

/// The terminal width for the update-note render (`process.stdout.columns
/// ?? 80`, package-manager-cli.ts:456).
fn stdout_columns() -> usize {
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
                && size.ws_col > 0
            {
                return usize::from(size.ws_col);
            }
        }
    }
    80
}

/// `printSelfUpdateNote` (package-manager-cli.ts:447-465): Markdown
/// rendered at the terminal width; parse/render failures fall back to the
/// raw note. Headless stdout is plain text (identity theme; upstream chalk
/// styling is TTY-only).
pub fn print_self_update_note(note: &str) {
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return;
    }
    println!();
    println!("Update note");
    let width = stdout_columns().max(20);
    let markdown = Markdown::new(
        trimmed,
        0,
        0,
        Arc::new(MarkdownTheme::identity()),
        None,
        None,
    );
    let rendered = markdown.render(width);
    if rendered.is_empty() {
        println!("{trimmed}");
    } else {
        for line in &rendered {
            println!("{}", line.trim_end());
        }
    }
    println!();
}

/// `handlePackageCommand` for `rpi update` (package-manager-cli.ts:702-735
/// and the update branch at :818-879). Returns the process exit code.
/// `args[0]` must be `"update"`.
pub async fn run_update(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let agent_dir = crate::config::get_agent_dir();
    run_update_in(args, &cwd, &agent_dir, None, None).await
}

/// [`run_update`] against explicit directories with injectable command
/// runner and release transport (tests inject fakes so no real `~/.rpi`,
/// process, or network is touched).
pub async fn run_update_in(
    args: &[String],
    cwd: &Path,
    agent_dir: &Path,
    runner: Option<Arc<dyn PackageCommandRunner>>,
    transport: Option<Arc<dyn LatestVersionTransport>>,
) -> i32 {
    let parsed = parse_update_args(args);
    if parsed.help {
        print!("{}", update_help());
        return 0;
    }
    if let Some(option) = &parsed.invalid_option {
        eprintln!("Unknown option {option} for \"update\".");
        eprintln!("Use \"{APP_NAME} --help\" or \"{UPDATE_USAGE}\".");
        return 1;
    }
    if let Some(option) = &parsed.missing_option_value {
        eprintln!("Missing value for {option}.");
        eprintln!("Usage: {UPDATE_USAGE}");
        return 1;
    }
    if let Some(argument) = &parsed.invalid_argument {
        eprintln!("Unexpected argument {argument}.");
        eprintln!("Usage: {UPDATE_USAGE}");
        return 1;
    }
    if let Some(conflict) = &parsed.conflicting_options {
        eprintln!("{conflict}");
        eprintln!("Usage: {UPDATE_USAGE}");
        return 1;
    }
    let Some(target) = parsed.target else {
        return 1;
    };
    if target == UpdateTarget::Models {
        return match refresh_model_catalogs().await {
            Ok(()) => {
                println!("Model catalogs refreshed");
                0
            }
            Err(message) => {
                eprintln!("Error: {message}");
                1
            }
        };
    }

    // `createCommandSettingsManager` with `useSavedProjectTrustOnly: true`
    // (package-manager-cli.ts:555-569, 740-746): no prompt, no extension
    // event — the override, else the stored trust.json decision.
    let mut settings_manager = SettingsManager::create(
        cwd,
        Some(agent_dir),
        SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    let trust_store = ProjectTrustStore::new(agent_dir);
    let saved_project_trusted = match trust_store.get(cwd) {
        Ok(decision) => decision == Some(true),
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    settings_manager.set_project_trusted(
        parsed
            .project_trust_override
            .unwrap_or(saved_project_trusted),
    );

    // `reportSettingsErrors` (package-manager-cli.ts:69-77, 753).
    for error in settings_manager.drain_errors() {
        eprintln!(
            "Warning (package command, {} settings): {}",
            error.scope.as_str(),
            error.error
        );
    }
    let npm_command = settings_manager.get_npm_command();
    // T14-W6a (ADR-0002 §8): resolve the version-check endpoint before the
    // settings manager moves into the package manager below.
    let version_check_endpoint =
        version_check_endpoint(settings_manager.get_version_check_url().as_deref());

    let runner = runner.unwrap_or_else(|| Arc::new(SystemPackageCommandRunner));
    let transport = transport.unwrap_or_else(|| Arc::new(ReqwestLatestVersionTransport));

    if parsed.show_extensions_skipped_note {
        println!(
            "Extensions are skipped. Run {APP_NAME} update --extensions to update extensions."
        );
    }

    if matches!(target, UpdateTarget::All | UpdateTarget::Extensions { .. }) {
        let update_source = match &target {
            UpdateTarget::Extensions { source } => source.clone(),
            _ => None,
        };
        let mut manager = DefaultPackageManager::with_options(
            crate::core::package_manager::PackageManagerOptions {
                cwd: cwd.to_path_buf(),
                agent_dir: agent_dir.to_path_buf(),
                settings_manager,
                runner: Some(runner.clone()),
                offline: None,
            },
        );
        manager.set_progress_callback(Some(Box::new(|event| {
            // Upstream prints `start` messages dim to stdout
            // (package-manager-cli.ts:758-762).
            if event.kind == crate::core::package_manager::ProgressKind::Start {
                if let Some(message) = &event.message {
                    println!("{message}");
                }
            }
        })));
        if let Err(message) = manager.update(update_source.as_deref()) {
            eprintln!("Error: {message}");
            return 1;
        }
        match &update_source {
            Some(source) => println!("Updated {source}"),
            None => println!("Updated packages"),
        }
    }

    if matches!(target, UpdateTarget::All | UpdateTarget::Self_) {
        let plan = match get_self_update_plan(
            parsed.force,
            transport.as_ref(),
            version_check_endpoint.as_deref(),
        )
        .await
        {
            Ok(plan) => plan,
            Err(message) => {
                eprintln!("Error: {message}");
                return 1;
            }
        };
        if !plan.should_run {
            return 0;
        }
        let install_method = detect_install_method();
        if cfg!(windows) && !matches!(install_method, InstallMethod::Npm | InstallMethod::Pnpm) {
            eprintln!(
                "{APP_NAME} self-update on Windows is only supported for npm and pnpm installs."
            );
            eprintln!(
                "Detected install method: {}. Update {APP_NAME} manually.",
                install_method.as_str()
            );
            return 1;
        }
        let update_target = SelfUpdatePackageTarget {
            package_name: plan.package_name.clone(),
            install_spec: plan.install_spec.clone(),
        };
        let Some(command) = get_self_update_command(
            PACKAGE_NAME,
            npm_command.as_deref(),
            &update_target,
            &system_command_probe,
        ) else {
            print_self_update_unavailable(npm_command.as_deref(), &update_target);
            return 1;
        };
        if let Some(note) = &plan.note {
            print_self_update_note(note);
        }
        // `prepareWindowsNpmSelfUpdate` (package-manager-cli.ts:526-534) is
        // a win32-only native-dependency quarantine; Windows is not a v0.1
        // target, so there is nothing to prepare here.
        if let Err(message) = run_self_update(&command, runner.as_ref()) {
            eprintln!("Error: {message}");
            if install_method == InstallMethod::Pnpm {
                eprintln!(
                    "If pnpm reports missing package versions, its cached registry metadata may be stale."
                );
                eprintln!("Run `pnpm store prune` and retry `{APP_NAME} update --self`.");
            }
            eprintln!(
                "If this keeps failing, run this command yourself: {}",
                command.display
            );
            return 1;
        }
        println!("Updated {APP_NAME} from {VERSION} to {}", plan.version);
    }
    0
}

// ---------------------------------------------------------------------------
// install / remove (uninstall) / list (package-manager-cli.ts:79-187,
// 189-316 non-update branches, 676-816)
// ---------------------------------------------------------------------------

/// `getPackageCommandUsage("install")` (package-manager-cli.ts:81-82).
pub const INSTALL_USAGE: &str = "rpi install <source> [-l] [--approve|--no-approve]";
/// `getPackageCommandUsage("remove")` (package-manager-cli.ts:83-84).
pub const REMOVE_USAGE: &str = "rpi remove <source> [-l] [--approve|--no-approve]";
/// `getPackageCommandUsage("list")` (package-manager-cli.ts:87-88).
pub const LIST_USAGE: &str = "rpi list [--approve|--no-approve]";

/// `PackageCommand` minus `update` (package-manager-cli.ts:33); the
/// `uninstall` alias normalizes to [`PackageCommandKind::Remove`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageCommandKind {
    Install,
    Remove,
    List,
}

impl PackageCommandKind {
    /// The name used in diagnostics (upstream reports `uninstall` as
    /// `"remove"`, package-manager-cli.ts:192-193).
    pub fn as_str(self) -> &'static str {
        match self {
            PackageCommandKind::Install => "install",
            PackageCommandKind::Remove => "remove",
            PackageCommandKind::List => "list",
        }
    }

    pub fn usage(self) -> &'static str {
        match self {
            PackageCommandKind::Install => INSTALL_USAGE,
            PackageCommandKind::Remove => REMOVE_USAGE,
            PackageCommandKind::List => LIST_USAGE,
        }
    }
}

/// `PackageCommandOptions` for install/remove/list
/// (package-manager-cli.ts:54-67, non-update fields).
#[derive(Debug)]
pub struct ParsedPackageCommand {
    pub command: PackageCommandKind,
    pub source: Option<String>,
    pub local: bool,
    pub project_trust_override: Option<bool>,
    pub help: bool,
    pub invalid_option: Option<String>,
    pub invalid_argument: Option<String>,
    pub missing_option_value: Option<String>,
    pub conflicting_options: Option<String>,
}

/// `parsePackageCommand` (package-manager-cli.ts:189-387) for
/// install/remove/uninstall/list. Returns `None` when `args[0]` is not one
/// of those commands. Update-only flags are `invalidOption` here.
pub fn parse_package_command(args: &[String]) -> Option<ParsedPackageCommand> {
    let raw_command = args.first()?;
    let command = match raw_command.as_str() {
        "install" => PackageCommandKind::Install,
        "remove" | "uninstall" => PackageCommandKind::Remove,
        "list" => PackageCommandKind::List,
        _ => return None,
    };
    let rest = &args[1..];

    let mut local = false;
    let mut project_trust_override: Option<bool> = None;
    let mut source: Option<String> = None;
    let mut parsed = ParsedPackageCommand {
        command,
        source: None,
        local: false,
        project_trust_override: None,
        help: false,
        invalid_option: None,
        invalid_argument: None,
        missing_option_value: None,
        conflicting_options: None,
    };

    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "-h" || arg == "--help" {
            parsed.help = true;
        } else if arg == "-l" || arg == "--local" {
            // Valid for install/remove only (package-manager-cli.ts:223-230).
            if command != PackageCommandKind::List {
                local = true;
            } else {
                parsed.invalid_option.get_or_insert_with(|| arg.clone());
            }
        } else if arg == "--self"
            || arg == "--extensions"
            || arg == "--models"
            || arg == "--all"
            || arg == "--force"
        {
            // Update-only flags (package-manager-cli.ts:232-285).
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if arg == "--approve" || arg == "-a" {
            project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            project_trust_override = Some(false);
        } else if arg == "--extension" {
            // Update-only; upstream does NOT consume a following value
            // (package-manager-cli.ts:287-289).
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if arg.starts_with('-') {
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if source.is_none() {
            source = Some(arg.clone());
        } else {
            parsed.invalid_argument.get_or_insert_with(|| arg.clone());
        }
        index += 1;
    }

    parsed.source = source;
    parsed.local = local;
    parsed.project_trust_override = project_trust_override;
    Some(parsed)
}

/// `printPackageCommandHelp` for install/remove/list
/// (package-manager-cli.ts:109-187), plain text.
pub fn package_command_help(command: PackageCommandKind) -> String {
    let config_dir = crate::config::CONFIG_DIR_NAME;
    match command {
        PackageCommandKind::Install => format!(
            r#"Usage:
  {INSTALL_USAGE}

Install a package and add it to settings.

Options:
  -l, --local       Install project-locally ({config_dir}/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  {APP_NAME} install npm:@foo/bar
  {APP_NAME} install git:github.com/user/repo
  {APP_NAME} install git:git@github.com:user/repo
  {APP_NAME} install https://github.com/user/repo
  {APP_NAME} install ssh://git@github.com/user/repo
  {APP_NAME} install ./local/path
"#
        ),
        PackageCommandKind::Remove => format!(
            r#"Usage:
  {REMOVE_USAGE}

Remove a package and its source from settings.
Alias: {APP_NAME} uninstall <source> [-l]

Options:
  -l, --local       Remove from project settings ({config_dir}/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  {APP_NAME} remove npm:@foo/bar
  {APP_NAME} uninstall npm:@foo/bar
"#
        ),
        PackageCommandKind::List => format!(
            r#"Usage:
  {LIST_USAGE}

List installed packages from user and project settings.

Options:
  -a, --approve      Trust project-local files for this command
  -na, --no-approve  Ignore project-local files for this command
"#
        ),
    }
}

/// The `list` branch output (package-manager-cli.ts:782-815): User/Project
/// groups, `(filtered)` markers and installed paths; a blank line
/// separates the groups.
pub fn format_package_list(packages: &[ConfiguredPackage]) -> String {
    if packages.is_empty() {
        return "No packages installed.\n".to_string();
    }
    let format_package = |pkg: &ConfiguredPackage| {
        let mut out = String::new();
        if pkg.filtered {
            out.push_str(&format!("  {} (filtered)\n", pkg.source));
        } else {
            out.push_str(&format!("  {}\n", pkg.source));
        }
        if let Some(path) = &pkg.installed_path {
            out.push_str(&format!("    {}\n", path.display()));
        }
        out
    };
    let user: Vec<&ConfiguredPackage> = packages
        .iter()
        .filter(|pkg| pkg.scope == SourceScope::User)
        .collect();
    let project: Vec<&ConfiguredPackage> = packages
        .iter()
        .filter(|pkg| pkg.scope == SourceScope::Project)
        .collect();
    let mut out = String::new();
    if !user.is_empty() {
        out.push_str("User packages:\n");
        for pkg in &user {
            out.push_str(&format_package(pkg));
        }
    }
    if !project.is_empty() {
        if !user.is_empty() {
            out.push('\n');
        }
        out.push_str("Project packages:\n");
        for pkg in &project {
            out.push_str(&format_package(pkg));
        }
    }
    out
}

/// Execute one parsed install/remove/list command against an already
/// trust-resolved manager (the testable core of [`run_package_command`]).
/// Returns the process exit code.
fn execute_package_command(
    parsed: &ParsedPackageCommand,
    manager: &mut DefaultPackageManager,
) -> i32 {
    match parsed.command {
        PackageCommandKind::Install => {
            let Some(source) = &parsed.source else {
                eprintln!("Missing {} source.", parsed.command.as_str());
                eprintln!("Usage: {}", parsed.command.usage());
                return 1;
            };
            match manager.install_and_persist(source, parsed.local) {
                Ok(()) => {
                    println!("Installed {source}");
                    0
                }
                Err(message) => {
                    eprintln!("Error: {message}");
                    1
                }
            }
        }
        PackageCommandKind::Remove => {
            let Some(source) = &parsed.source else {
                eprintln!("Missing {} source.", parsed.command.as_str());
                eprintln!("Usage: {}", parsed.command.usage());
                return 1;
            };
            match manager.remove_and_persist(source, parsed.local) {
                Ok(true) => {
                    println!("Removed {source}");
                    0
                }
                Ok(false) => {
                    eprintln!("No matching package found for {source}");
                    1
                }
                Err(message) => {
                    eprintln!("Error: {message}");
                    1
                }
            }
        }
        PackageCommandKind::List => {
            print!(
                "{}",
                format_package_list(&manager.list_configured_packages())
            );
            0
        }
    }
}

/// `handlePackageCommand` for install/remove/uninstall/list
/// (package-manager-cli.ts:676-816 minus the update branch).
/// `args[0]` must be one of those commands. Returns the exit code.
pub fn run_package_command(args: &[String]) -> i32 {
    run_package_command_with(args, None)
}

/// [`run_package_command`] with an injectable command runner (tests).
pub fn run_package_command_with(
    args: &[String],
    runner: Option<Arc<dyn PackageCommandRunner>>,
) -> i32 {
    let Some(parsed) = parse_package_command(args) else {
        return 1;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let agent_dir = crate::config::get_agent_dir();
    run_package_command_in(&parsed, &cwd, &agent_dir, runner)
}

/// [`run_package_command`] against explicit directories (tests inject a
/// temporary cwd/agent dir here so no real `~/.rpi` is touched).
pub fn run_package_command_in(
    parsed: &ParsedPackageCommand,
    cwd: &Path,
    agent_dir: &Path,
    runner: Option<Arc<dyn PackageCommandRunner>>,
) -> i32 {
    if parsed.help {
        print!("{}", package_command_help(parsed.command));
        return 0;
    }
    if let Some(option) = &parsed.invalid_option {
        eprintln!(
            "Unknown option {option} for \"{}\".",
            parsed.command.as_str()
        );
        eprintln!(
            "Use \"{APP_NAME} --help\" or \"{}\".",
            parsed.command.usage()
        );
        return 1;
    }
    if let Some(option) = &parsed.missing_option_value {
        eprintln!("Missing value for {option}.");
        eprintln!("Usage: {}", parsed.command.usage());
        return 1;
    }
    if let Some(argument) = &parsed.invalid_argument {
        eprintln!("Unexpected argument {argument}.");
        eprintln!("Usage: {}", parsed.command.usage());
        return 1;
    }
    if let Some(conflict) = &parsed.conflicting_options {
        eprintln!("{conflict}");
        eprintln!("Usage: {}", parsed.command.usage());
        return 1;
    }
    if parsed.command != PackageCommandKind::List && parsed.source.is_none() {
        eprintln!("Missing {} source.", parsed.command.as_str());
        eprintln!("Usage: {}", parsed.command.usage());
        return 1;
    }

    // `createCommandSettingsManager` (package-manager-cli.ts:555-601),
    // headless: no trust extensions and no UI prompt, so trust comes from
    // the override, trust.json, or `defaultProjectTrust` (ask → false).
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
            return 1;
        }
    };
    settings_manager.set_project_trusted(project_trusted);

    // `writesProjectPackageConfig` (package-manager-cli.ts:739, 748-752).
    let writes_project_package_config = parsed.command != PackageCommandKind::List && parsed.local;
    if writes_project_package_config && !settings_manager.is_project_trusted() {
        eprintln!("Project is not trusted. Use --approve to modify local package config.");
        return 1;
    }

    // `reportSettingsErrors` (package-manager-cli.ts:69-77, 753).
    for error in settings_manager.drain_errors() {
        eprintln!(
            "Warning (package command, {} settings): {}",
            error.scope.as_str(),
            error.error
        );
    }

    let mut manager =
        DefaultPackageManager::with_options(crate::core::package_manager::PackageManagerOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            settings_manager,
            runner,
            offline: None,
        });
    manager.set_progress_callback(Some(Box::new(|event| {
        // Upstream prints `start` messages dim to stdout
        // (package-manager-cli.ts:758-762).
        if event.kind == crate::core::package_manager::ProgressKind::Start {
            if let Some(message) = &event.message {
                println!("{message}");
            }
        }
    })));

    execute_package_command(parsed, &mut manager)
}

#[cfg(test)]
mod tests {
    //! Port of the update-command intent of
    //! `packages/coding-agent/test/package-manager.test.ts` arg validation.

    use super::*;

    #[test]
    fn usage_lines_start_with_app_name() {
        // T14 review N-1: the usage strings carry the literal `rpi`; bind
        // them to `APP_NAME` so a rename cannot leave the help text stale.
        for usage in [UPDATE_USAGE, INSTALL_USAGE, REMOVE_USAGE, LIST_USAGE] {
            assert!(
                usage.starts_with(&format!("{APP_NAME} ")),
                "usage line must start with {{APP_NAME}}: {usage}"
            );
        }
    }

    fn parse(input: &[&str]) -> ParsedUpdate {
        parse_update_args(&input.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn target(input: &[&str]) -> Option<UpdateTarget> {
        parse(input).target
    }

    #[test]
    fn update_models_target_parses() {
        assert_eq!(target(&["update", "--models"]), Some(UpdateTarget::Models));
    }

    #[test]
    fn update_models_rejects_combined_targets() {
        // `--all` conflicts are reported first (package-manager-cli.ts:
        // 321-327 run before the `--models` check at :329-336).
        let parsed = parse(&["update", "--models", "--all"]);
        assert_eq!(parsed.target, Some(UpdateTarget::Models));
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--all cannot be combined with --self, --extensions, --models, or --extension")
        );
        let parsed = parse(&["update", "--models", "--self"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--models cannot be combined with --self, --extensions, --all, or --extension")
        );
        let parsed = parse(&["update", "--models", "my-package"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--models cannot be combined with a positional source")
        );
        let parsed = parse(&["update", "--models", "--extension", "foo"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--models cannot be combined with --self, --extensions, --all, or --extension")
        );
    }

    #[test]
    fn update_defaults_to_self() {
        assert_eq!(target(&["update"]), Some(UpdateTarget::Self_));
        assert_eq!(target(&["update", "pi"]), Some(UpdateTarget::Self_));
        assert_eq!(target(&["update", "self"]), Some(UpdateTarget::Self_));
    }

    #[test]
    fn update_flag_combinations() {
        assert_eq!(
            target(&["update", "--self", "--extensions"]),
            Some(UpdateTarget::All)
        );
        assert_eq!(target(&["update", "--all"]), Some(UpdateTarget::All));
        assert_eq!(
            target(&["update", "--extensions"]),
            Some(UpdateTarget::Extensions { source: None })
        );
        assert_eq!(
            target(&["update", "--extension", "npm:@foo/bar"]),
            Some(UpdateTarget::Extensions {
                source: Some("npm:@foo/bar".to_owned())
            })
        );
        assert_eq!(
            target(&["update", "my-package"]),
            Some(UpdateTarget::Extensions {
                source: Some("my-package".to_owned())
            })
        );
    }

    #[test]
    fn update_reports_invalid_options_and_arguments() {
        // `-l` is install/remove-only (package-manager-cli.ts:223-230).
        let parsed = parse(&["update", "-l"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("-l"));
        // Unknown flag.
        let parsed = parse(&["update", "--bogus"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("--bogus"));
        // Second positional is an invalid argument.
        let parsed = parse(&["update", "a", "b"]);
        assert_eq!(parsed.invalid_argument.as_deref(), Some("b"));
        // --extension without a value.
        let parsed = parse(&["update", "--extension"]);
        assert_eq!(parsed.missing_option_value.as_deref(), Some("--extension"));
        let parsed = parse(&["update", "--extension", "--models"]);
        assert_eq!(parsed.missing_option_value.as_deref(), Some("--extension"));
    }

    #[test]
    fn update_accepts_help_and_trust_flags() {
        assert!(parse(&["update", "--help"]).help);
        assert!(parse(&["update", "-h"]).help);
        // --approve/--no-approve/--force are accepted on update.
        let parsed = parse(&["update", "--models", "--approve", "--force"]);
        assert_eq!(parsed.target, Some(UpdateTarget::Models));
        assert!(parsed.conflicting_options.is_none());
    }

    #[test]
    fn update_help_text_mentions_models() {
        let help = update_help();
        assert!(help.contains("--models                Refresh model catalogs only"));
        assert!(help.contains("rpi update --models       Refresh model catalogs only"));
    }
}

#[cfg(test)]
mod package_command_tests {
    //! Port of the install/remove/list intent of
    //! `packages/coding-agent/test/package-command-paths.test.ts` and the
    //! non-update branches of `parsePackageCommand` arg validation.

    use super::*;
    use crate::core::package_manager::CommandRequest;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirs {
        root: PathBuf,
        cwd: PathBuf,
        agent_dir: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let unique = format!(
                "rpi-pkg-cmd-test-{}-{}",
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

    /// Any command execution is a bug in these tests (local sources never
    /// spawn); captures fail loudly.
    struct NoSpawnRunner {
        calls: Mutex<Vec<String>>,
    }

    impl PackageCommandRunner for NoSpawnRunner {
        fn run(&self, request: &CommandRequest) -> Result<(), String> {
            self.calls.lock().unwrap().push(request.display());
            Err("unexpected command".to_string())
        }
        fn run_capture(&self, request: &CommandRequest) -> Result<String, String> {
            self.calls.lock().unwrap().push(request.display());
            Ok(String::new())
        }
    }

    fn parse(input: &[&str]) -> ParsedPackageCommand {
        parse_package_command(&input.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    // ---- parse (parsePackageCommand non-update branches) ----

    #[test]
    fn test_parse_install_source_and_flags() {
        let parsed = parse(&["install", "npm:@foo/bar"]);
        assert_eq!(parsed.command, PackageCommandKind::Install);
        assert_eq!(parsed.source.as_deref(), Some("npm:@foo/bar"));
        assert!(!parsed.local);

        let parsed = parse(&["install", "./pkg", "-l", "-a"]);
        assert!(parsed.local);
        assert_eq!(parsed.project_trust_override, Some(true));

        let parsed = parse(&["install", "./pkg", "--local", "--no-approve"]);
        assert!(parsed.local);
        assert_eq!(parsed.project_trust_override, Some(false));

        let parsed = parse(&["install", "./pkg", "-na"]);
        assert_eq!(parsed.project_trust_override, Some(false));
    }

    #[test]
    fn test_parse_uninstall_alias_maps_to_remove() {
        let parsed = parse(&["uninstall", "npm:@foo/bar"]);
        assert_eq!(parsed.command, PackageCommandKind::Remove);
        assert_eq!(parsed.source.as_deref(), Some("npm:@foo/bar"));
    }

    #[test]
    fn test_parse_list_rejects_local_flag() {
        let parsed = parse(&["list", "-l"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("-l"));
        // Upstream parses a positional as `source` on every command and
        // the list branch silently ignores it
        // (package-manager-cli.ts:309-314).
        let parsed = parse(&["list", "foo"]);
        assert_eq!(parsed.source.as_deref(), Some("foo"));
        assert!(parsed.invalid_argument.is_none());
        let parsed = parse(&["list", "foo", "bar"]);
        assert_eq!(parsed.invalid_argument.as_deref(), Some("bar"));
    }

    #[test]
    fn test_parse_update_only_flags_are_invalid_for_install() {
        for flag in [
            "--self",
            "--extensions",
            "--models",
            "--all",
            "--force",
            "--extension",
        ] {
            let parsed = parse(&["install", "npm:foo", flag]);
            assert_eq!(parsed.invalid_option.as_deref(), Some(flag), "{flag}");
        }
        // `--extension` does not consume a value on non-update commands;
        // the value becomes the (first free) positional.
        let parsed = parse(&["install", "--extension", "npm:foo"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("--extension"));
        assert_eq!(parsed.source.as_deref(), Some("npm:foo"));
    }

    #[test]
    fn test_parse_reports_extra_positional_and_unknown_options() {
        let parsed = parse(&["remove", "a", "b"]);
        assert_eq!(parsed.invalid_argument.as_deref(), Some("b"));
        let parsed = parse(&["install", "--bogus", "npm:foo"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("--bogus"));
    }

    #[test]
    fn test_parse_help() {
        assert!(parse(&["install", "--help"]).help);
        assert!(parse(&["remove", "-h"]).help);
        assert!(parse(&["list", "--help"]).help);
    }

    #[test]
    fn test_parse_non_package_command_returns_none() {
        assert!(parse_package_command(&["update".to_string()]).is_none());
        assert!(parse_package_command(&["config".to_string()]).is_none());
    }

    // ---- help text ----

    #[test]
    fn test_help_texts_carry_usage_and_examples() {
        let install = package_command_help(PackageCommandKind::Install);
        assert!(install.contains(INSTALL_USAGE));
        assert!(install.contains("rpi install npm:@foo/bar"));
        assert!(install.contains("-l, --local"));
        let remove = package_command_help(PackageCommandKind::Remove);
        assert!(remove.contains(REMOVE_USAGE));
        assert!(remove.contains("rpi uninstall <source> [-l]"));
        let list = package_command_help(PackageCommandKind::List);
        assert!(list.contains(LIST_USAGE));
    }

    // ---- list formatting (package-manager-cli.ts:782-815) ----

    #[test]
    fn test_format_package_list_empty() {
        assert_eq!(format_package_list(&[]), "No packages installed.\n");
    }

    #[test]
    fn test_format_package_list_groups_and_marks_filtered() {
        let packages = vec![
            ConfiguredPackage {
                source: "npm:left-pad".to_string(),
                scope: SourceScope::User,
                filtered: false,
                installed_path: Some(PathBuf::from(
                    "/home/u/.rpi/agent/npm/node_modules/left-pad",
                )),
            },
            ConfiguredPackage {
                source: "npm:filtered-pkg".to_string(),
                scope: SourceScope::User,
                filtered: true,
                installed_path: None,
            },
            ConfiguredPackage {
                source: "git:github.com/user/repo".to_string(),
                scope: SourceScope::Project,
                filtered: false,
                installed_path: None,
            },
        ];
        let output = format_package_list(&packages);
        assert_eq!(
            output,
            "User packages:\n  npm:left-pad\n    /home/u/.rpi/agent/npm/node_modules/left-pad\n  npm:filtered-pkg (filtered)\n\nProject packages:\n  git:github.com/user/repo\n"
        );
    }

    // ---- command execution (handlePackageCommand paths) ----

    #[test]
    fn test_install_local_package_persists_relative_source() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join("pkg/extensions")).unwrap();
        std::fs::write(dirs.cwd.join("pkg/extensions/index.ts"), "").unwrap();
        let parsed = parse(&["install", "./pkg"]);
        let code = run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None);
        assert_eq!(code, 0);

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dirs.agent_dir.join("settings.json")).unwrap(),
        )
        .unwrap();
        // Stored relative to the agent dir (settings base).
        assert_eq!(settings["packages"][0].as_str().unwrap(), "../cwd/pkg");
    }

    #[test]
    fn test_install_local_initializes_fresh_project_settings() {
        // No trust-requiring resources: the project counts as trusted and
        // `-l` creates `.rpi/settings.json` (package-command-paths.test.ts
        // "allows local package install to initialize fresh project
        // settings").
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join("pkg/extensions")).unwrap();
        std::fs::write(dirs.cwd.join("pkg/extensions/index.ts"), "").unwrap();
        let parsed = parse(&["install", "./pkg", "-l"]);
        let code = run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None);
        assert_eq!(code, 0);

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dirs.cwd.join(".rpi/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["packages"][0].as_str().unwrap(), "../pkg");
    }

    #[test]
    fn test_install_local_blocked_when_project_untrusted() {
        // A pre-existing `.rpi/settings.json` requires trust; without
        // `-a` the headless default (ask → false) blocks the write.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join(".rpi")).unwrap();
        std::fs::write(dirs.cwd.join(".rpi/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(dirs.cwd.join("pkg/extensions")).unwrap();
        std::fs::write(dirs.cwd.join("pkg/extensions/index.ts"), "").unwrap();

        let parsed = parse(&["install", "./pkg", "-l"]);
        let code = run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None);
        assert_eq!(code, 1);
        assert_eq!(
            std::fs::read_to_string(dirs.cwd.join(".rpi/settings.json")).unwrap(),
            "{}"
        );

        // `-a` approves for this command.
        let parsed = parse(&["install", "./pkg", "-l", "-a"]);
        let code = run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None);
        assert_eq!(code, 0);
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dirs.cwd.join(".rpi/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["packages"][0].as_str().unwrap(), "../pkg");
    }

    #[test]
    fn test_remove_without_match_exits_1() {
        let dirs = TestDirs::new();
        let parsed = parse(&["remove", "npm:not-configured"]);
        let code = run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None);
        assert_eq!(code, 1);
    }

    #[test]
    fn test_remove_local_package_exits_0() {
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join("pkg/extensions")).unwrap();
        std::fs::write(dirs.cwd.join("pkg/extensions/index.ts"), "").unwrap();
        let parsed = parse(&["install", "./pkg"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            0
        );

        let parsed = parse(&["uninstall", "./pkg"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            0
        );
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dirs.agent_dir.join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["packages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_list_exits_0_with_and_without_packages() {
        let dirs = TestDirs::new();
        let parsed = parse(&["list"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            0
        );

        std::fs::create_dir_all(dirs.cwd.join("pkg/extensions")).unwrap();
        std::fs::write(dirs.cwd.join("pkg/extensions/index.ts"), "").unwrap();
        let parsed = parse(&["install", "./pkg"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            0
        );
        let parsed = parse(&["list"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            0
        );
    }

    #[test]
    fn test_missing_source_exits_1() {
        let dirs = TestDirs::new();
        let parsed = parse(&["install"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            1
        );
        let parsed = parse(&["remove"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            1
        );
    }

    #[test]
    fn test_unknown_option_exits_1() {
        let dirs = TestDirs::new();
        let parsed = parse(&["install", "--bogus", "npm:foo"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("--bogus"));
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            1
        );
    }

    #[test]
    fn test_list_with_no_approve_ignores_project_packages() {
        // Project has packages configured but trust-requiring resources;
        // without approval the project settings stay invisible to `list`.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join(".rpi")).unwrap();
        std::fs::write(
            dirs.cwd.join(".rpi/settings.json"),
            r#"{"packages": ["npm:project-pkg"]}"#,
        )
        .unwrap();
        let parsed = parse(&["list", "-na"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, None),
            0
        );
        // The project scope stays untrusted: its settings are treated as
        // empty (the SettingsManager gate), so only user packages list.
    }

    #[test]
    fn test_no_spawn_runner_unused_for_local_flows() {
        // Guards against accidental command execution in local install /
        // remove / list flows.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join("pkg/extensions")).unwrap();
        std::fs::write(dirs.cwd.join("pkg/extensions/index.ts"), "").unwrap();
        let runner = Arc::new(NoSpawnRunner {
            calls: Mutex::new(Vec::new()),
        });
        let parsed = parse(&["install", "./pkg"]);
        assert_eq!(
            run_package_command_in(&parsed, &dirs.cwd, &dirs.agent_dir, Some(runner.clone())),
            0
        );
        assert!(runner.calls.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod update_cli_tests {
    //! T14-W3: the `update` branch of `handlePackageCommand`
    //! (package-manager-cli.ts:726-879) — full target matrix, trust
    //! (saved-only), extensions update and the self-update plan, all with
    //! fake runner / scripted release transport (no process, no network).

    use super::*;
    use crate::core::package_manager::SystemPackageCommandRunner;
    use crate::core::version_check::LatestVersionTransport;
    use futures::future::BoxFuture;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirs {
        root: PathBuf,
        cwd: PathBuf,
        agent_dir: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let unique = format!(
                "rpi-update-cli-test-{}-{}",
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

    type CaptureHandler = Box<dyn Fn(&CommandRequest) -> Result<String, String> + Send + Sync>;

    struct FakeRunner {
        calls: Mutex<Vec<String>>,
        handler: CaptureHandler,
    }

    impl FakeRunner {
        fn npm_view_latest() -> Arc<Self> {
            Arc::new(FakeRunner {
                calls: Mutex::new(Vec::new()),
                handler: Box::new(|request| {
                    if request.args.first().map(String::as_str) == Some("view") {
                        return Ok(r#""9.9.9""#.to_string());
                    }
                    Ok(String::new())
                }),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PackageCommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<(), String> {
            self.calls.lock().unwrap().push(request.display());
            (self.handler)(request).map(|_| ())
        }
        fn run_capture(&self, request: &CommandRequest) -> Result<String, String> {
            self.calls.lock().unwrap().push(request.display());
            (self.handler)(request)
        }
    }

    /// Scripted release transport; records the probed URLs.
    struct StubTransport {
        response: Result<Option<String>, String>,
        calls: Mutex<Vec<String>>,
    }

    impl StubTransport {
        fn responds(response: Result<Option<String>, String>) -> Arc<Self> {
            Arc::new(StubTransport {
                response,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn newer_version() -> Arc<Self> {
            Self::responds(Ok(Some(r#"{"version": "99.0.0"}"#.to_string())))
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn urls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl LatestVersionTransport for StubTransport {
        fn get<'a>(
            &'a self,
            url: &'a str,
            _user_agent: &'a str,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Option<String>, String>> {
            self.calls.lock().unwrap().push(url.to_string());
            let response = self.response.clone();
            Box::pin(async move { response })
        }
    }

    fn args(input: &[&str]) -> Vec<String> {
        input.iter().map(|s| s.to_string()).collect()
    }

    // ---- parser: --all conflict matrix + flag capture (W6-C gap closed in W3) ----

    fn parse(input: &[&str]) -> ParsedUpdate {
        parse_update_args(&args(input))
    }

    #[test]
    fn update_all_conflict_matrix() {
        // package-manager-cli.ts:321-327.
        let parsed = parse(&["update", "--all", "--self"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--all cannot be combined with --self, --extensions, --models, or --extension")
        );
        let parsed = parse(&["update", "--all", "--extensions"]);
        assert!(parsed.conflicting_options.is_some());
        let parsed = parse(&["update", "--all", "--models"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--all cannot be combined with --self, --extensions, --models, or --extension")
        );
        let parsed = parse(&["update", "--all", "--extension", "foo"]);
        assert!(parsed.conflicting_options.is_some());
        let parsed = parse(&["update", "--all", "foo"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--all cannot be combined with a positional source")
        );
        // ...but `--self --extensions` is the long form of `--all`
        // (package-manager-cli.ts:361-362) and stays legal.
        let parsed = parse(&["update", "--self", "--extensions"]);
        assert!(parsed.conflicting_options.is_none());
        assert_eq!(parsed.target, Some(UpdateTarget::All));
    }

    #[test]
    fn update_captures_force_and_trust_flags() {
        let parsed = parse(&["update", "--self", "--force"]);
        assert!(parsed.force);
        let parsed = parse(&["update", "-a"]);
        assert_eq!(parsed.project_trust_override, Some(true));
        let parsed = parse(&["update", "-na"]);
        assert_eq!(parsed.project_trust_override, Some(false));
        // Bare update shows the extensions-skipped note
        // (package-manager-cli.ts:368-369); explicit targets do not.
        assert!(parse(&["update"]).show_extensions_skipped_note);
        assert!(!parse(&["update", "--self"]).show_extensions_skipped_note);
        assert!(!parse(&["update", "pi"]).show_extensions_skipped_note);
        assert!(!parse(&["update", "--all"]).show_extensions_skipped_note);
    }

    // ---- extensions target ----

    #[tokio::test]
    async fn update_extensions_runs_npm_install_and_skips_self() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::npm_view_latest();
        let transport = StubTransport::newer_version();
        // Configure one user package.
        let mut settings_manager = SettingsManager::create(
            &dirs.cwd,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: false,
            },
        );
        settings_manager.set_packages(vec![crate::core::settings_manager::PackageSource::Source(
            "npm:foo".to_string(),
        )]);
        drop(settings_manager);

        let code = run_update_in(
            &args(&["update", "--extensions"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner.clone()),
            Some(transport.clone()),
        )
        .await;
        assert_eq!(code, 0);
        assert!(runner
            .calls()
            .iter()
            .any(|call| call.contains("install") && call.contains("foo@latest")));
        // The self target never ran: no release probe.
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn update_single_extension_by_source() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::npm_view_latest();
        let transport = StubTransport::newer_version();
        let mut settings_manager = SettingsManager::create(
            &dirs.cwd,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: false,
            },
        );
        settings_manager.set_packages(vec![
            crate::core::settings_manager::PackageSource::Source("npm:foo".to_string()),
            crate::core::settings_manager::PackageSource::Source("npm:bar".to_string()),
        ]);
        drop(settings_manager);

        let code = run_update_in(
            &args(&["update", "npm:foo"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner.clone()),
            Some(transport.clone()),
        )
        .await;
        assert_eq!(code, 0);
        let installs: Vec<String> = runner
            .calls()
            .into_iter()
            .filter(|call| call.contains("install"))
            .collect();
        assert_eq!(installs.len(), 1);
        assert!(installs[0].contains("foo@latest"));

        // Unknown source → exit 1 (No matching package found).
        let runner = FakeRunner::npm_view_latest();
        let code = run_update_in(
            &args(&["update", "npm:nope"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner),
            Some(transport),
        )
        .await;
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn update_extensions_skips_untrusted_project_packages() {
        // `useSavedProjectTrustOnly: true`: without a trust.json entry the
        // project settings stay invisible (no prompt, ever).
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.cwd.join(".rpi")).unwrap();
        std::fs::write(
            dirs.cwd.join(".rpi/settings.json"),
            r#"{"packages": ["npm:project-pkg"]}"#,
        )
        .unwrap();
        let runner = FakeRunner::npm_view_latest();
        let code = run_update_in(
            &args(&["update", "--extensions"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner.clone()),
            Some(StubTransport::newer_version()),
        )
        .await;
        assert_eq!(code, 0);
        assert!(!runner
            .calls()
            .iter()
            .any(|call| call.contains("project-pkg")));

        // A stored trust decision makes the project packages visible.
        let trust_store = ProjectTrustStore::new(&dirs.agent_dir);
        trust_store.set(&dirs.cwd, Some(true)).unwrap();
        let runner = FakeRunner::npm_view_latest();
        let code = run_update_in(
            &args(&["update", "--extensions"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner.clone()),
            Some(StubTransport::newer_version()),
        )
        .await;
        assert_eq!(code, 0);
        assert!(runner
            .calls()
            .iter()
            .any(|call| call.contains("project-pkg")));
    }

    // ---- self target (the test binary is a standalone executable → the
    // Binary install method) ----

    #[tokio::test]
    async fn update_self_up_to_date_exits_0_without_runner() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::npm_view_latest();
        let transport = StubTransport::responds(Ok(Some(format!(r#"{{"version": "{VERSION}"}}"#))));
        let code = run_update_in(
            &args(&["update", "--self"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner.clone()),
            Some(transport),
        )
        .await;
        assert_eq!(code, 0);
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn update_self_newer_version_on_binary_install_is_unavailable() {
        let dirs = TestDirs::new();
        let transport = StubTransport::newer_version();
        let code = run_update_in(
            &args(&["update", "--self"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(FakeRunner::npm_view_latest()),
            Some(transport.clone()),
        )
        .await;
        // A standalone binary cannot self-update (upstream bun-binary
        // outcome): exit 1 after probing the release endpoint.
        assert_eq!(code, 1);
        assert_eq!(transport.call_count(), 1);
    }

    #[tokio::test]
    async fn update_self_force_rechecks_even_when_current() {
        let dirs = TestDirs::new();
        let transport = StubTransport::responds(Ok(Some(format!(r#"{{"version": "{VERSION}"}}"#))));
        // --force makes the plan run even at the latest version; the
        // binary install then reports unavailable → exit 1 proves the
        // force path was taken (without force this exits 0, see above).
        let code = run_update_in(
            &args(&["update", "--self", "--force"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(FakeRunner::npm_view_latest()),
            Some(transport),
        )
        .await;
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn update_self_endpoint_failures_exit_1() {
        let dirs = TestDirs::new();
        // Non-OK response → "Could not determine latest rpi version."
        let code = run_update_in(
            &args(&["update", "--self"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(FakeRunner::npm_view_latest()),
            Some(StubTransport::responds(Ok(None))),
        )
        .await;
        assert_eq!(code, 1);
        // Transport error → wrapped message, same exit code.
        let code = run_update_in(
            &args(&["update", "--self"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(FakeRunner::npm_view_latest()),
            Some(StubTransport::responds(Err("dns".to_string()))),
        )
        .await;
        assert_eq!(code, 1);
    }

    // ---- T14-W6a: configurable version-check endpoint (ADR-0002 §8) ----

    #[tokio::test]
    async fn update_self_probes_the_settings_configured_endpoint() {
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.agent_dir.join("settings.json"),
            r#"{"versionCheckUrl": "https://mirror.test/latest-version"}"#,
        )
        .expect("write settings");
        let transport = StubTransport::responds(Ok(Some(format!(r#"{{"version": "{VERSION}"}}"#))));
        let code = run_update_in(
            &args(&["update", "--self"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(FakeRunner::npm_view_latest()),
            Some(transport.clone()),
        )
        .await;
        // Current version → already up to date → exit 0.
        assert_eq!(code, 0);
        assert_eq!(
            transport.urls(),
            vec!["https://mirror.test/latest-version".to_string()]
        );
    }

    #[tokio::test]
    async fn update_self_disabled_endpoint_makes_no_request() {
        // Zero-network anchor: `versionCheckUrl: "off"` disables the probe
        // entirely — the release transport is never touched.
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.agent_dir.join("settings.json"),
            r#"{"versionCheckUrl": "off"}"#,
        )
        .expect("write settings");
        let transport = StubTransport::newer_version();
        let code = run_update_in(
            &args(&["update", "--self"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(FakeRunner::npm_view_latest()),
            Some(transport.clone()),
        )
        .await;
        // Same surface as an unreachable endpoint (upstream !ok → undefined).
        assert_eq!(code, 1);
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn update_all_runs_extensions_then_self() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::npm_view_latest();
        let transport = StubTransport::newer_version();
        let mut settings_manager = SettingsManager::create(
            &dirs.cwd,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: false,
            },
        );
        settings_manager.set_packages(vec![crate::core::settings_manager::PackageSource::Source(
            "npm:foo".to_string(),
        )]);
        drop(settings_manager);

        let code = run_update_in(
            &args(&["update", "--all"]),
            &dirs.cwd,
            &dirs.agent_dir,
            Some(runner.clone()),
            Some(transport.clone()),
        )
        .await;
        // Extensions update succeeded; self-update then failed on the
        // binary install (exit 1) — both halves ran.
        assert_eq!(code, 1);
        assert!(runner
            .calls()
            .iter()
            .any(|call| call.contains("foo@latest")));
        assert_eq!(transport.call_count(), 1);
    }

    #[test]
    fn system_runner_is_the_default() {
        // Compile-time wiring check: the production runner/transport
        // choices stay the system ones.
        let _runner: Arc<dyn PackageCommandRunner> = Arc::new(SystemPackageCommandRunner);
        let _transport: Arc<dyn LatestVersionTransport> =
            Arc::new(crate::core::version_check::ReqwestLatestVersionTransport);
    }
}
