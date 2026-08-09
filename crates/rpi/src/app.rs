//! Application startup pipeline (design §6.1).
//!
//! Port of `packages/coding-agent/src/main.ts` @ pi 0.82.1 (2efa728).
//!
//! T10 boundaries (task file §Out):
//! - Subcommand dispatch: `update` (all targets since T14-W3),
//!   install/remove/uninstall/list (T14-W2) and `config` (T14-W3) are real.
//! - First-time setup never runs in headless modes.
//! - `--export` exports the session file to HTML and exits (T14-W5,
//!   `core::export_html::export_from_file`).
//! - Migrations (`migrations.ts`) are permanently out of scope (ADR-0003 §3).
//! - HTTP proxy dispatcher configuration is T13; reqwest's built-in env
//!   proxy support covers `HTTP_PROXY`/`HTTPS_PROXY`.

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rpi_agent::types::ThinkingLevel;

use crate::cli::args::{parse_args, print_help, Args, Mode};
use crate::cli::diagnostics::{has_error, DiagnosticLevel};
use crate::cli::file_processor::{process_file_arguments, FileProcessError};
use crate::cli::initial_message::build_initial_message;
use crate::cli::list_models::list_models;
use crate::config::{
    get_agent_dir, resolve_session_dir_from_env, ENV_OFFLINE, ENV_SKIP_VERSION_CHECK, VERSION,
};
use crate::core::agent_session_runtime::{
    create_agent_session_runtime, CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
};
use crate::core::agent_session_services::AgentSessionRuntimeDiagnostic;
use crate::core::agent_session_services::{
    create_agent_session_services, CreateAgentSessionServicesOptions,
};
use crate::core::auth_guidance::format_no_models_available_message;
use crate::core::extensions::{SessionStartEvent, SessionStartReason};
use crate::core::model_resolver::{
    resolve_cli_model, resolve_model_scope_with_diagnostics, ResolveCliModelOptions, ScopedModel,
};
use crate::core::model_runtime::ModelRuntime;
use crate::core::session_cwd::{format_missing_session_cwd_error, get_missing_session_cwd_issue};
use crate::core::session_manager::{assert_valid_session_id, NewSessionOptions, SessionManager};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::trust_manager::{
    default_project_trust_from_settings, has_trust_requiring_project_resources,
    resolve_project_trusted, ProjectTrustContext, ProjectTrustStore,
};
use crate::error::RpiError;
use crate::modes::interactive::run_interactive_mode;
use crate::modes::print_mode::{run_print_mode, PrintModeOptions, PrintOutputMode};
use crate::modes::rpc::run_rpc_mode;
use crate::sdk::{create_agent_session, CreateAgentSessionOptions, NoTools};
use crate::tools::path_utils::resolve_path;

/// Upstream `EXTENSION_LOAD_FAILURE_HINT` (main.ts:52).
const EXTENSION_LOAD_FAILURE_HINT: &str = "Hint: Start without extensions using \"rpi -ne\".";

// Package/config subcommands dispatch before `parseArgs` (main.ts:492-507):
// `config` (`cli::config_command`, T14-W3), `update`
// (`cli::package_command::run_update`, W6-C/T14-W3) and
// install/remove/uninstall/list (`cli::package_command`, T14-W2).

fn is_truthy_env_flag(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(value) => {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
    }
}

/// `resolveAppMode` (main.ts:100-111).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

fn resolve_app_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    if parsed.mode == Some(Mode::Rpc) {
        return AppMode::Rpc;
    }
    if parsed.mode == Some(Mode::Json) {
        return AppMode::Json;
    }
    if parsed.print || !stdin_is_tty || !stdout_is_tty {
        return AppMode::Print;
    }
    AppMode::Interactive
}

/// `collectSettingsDiagnostics` (main.ts:77-85).
fn collect_settings_diagnostics(
    settings_manager: &mut SettingsManager,
    context: &str,
) -> Vec<AgentSessionRuntimeDiagnostic> {
    settings_manager
        .drain_errors()
        .into_iter()
        .map(|error| AgentSessionRuntimeDiagnostic {
            level: DiagnosticLevel::Warning,
            message: format!(
                "({context}, {} settings) {}",
                error.scope.as_str(),
                error.error
            ),
        })
        .collect()
}

/// `reportDiagnostics` (main.ts:87-93).
fn report_diagnostics(diagnostics: &[AgentSessionRuntimeDiagnostic], err: &mut dyn Write) {
    for diagnostic in diagnostics {
        let prefix = match diagnostic.level {
            DiagnosticLevel::Error => "Error: ",
            DiagnosticLevel::Warning => "Warning: ",
            DiagnosticLevel::Info => "",
        };
        let _ = writeln!(err, "{prefix}{}", diagnostic.message);
    }
}

/// `readPipedStdin` (main.ts:59-75).
fn read_piped_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut data = String::new();
    std::io::stdin().read_to_string(&mut data).ok()?;
    let trimmed = data.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// `promptConfirm` (main.ts:192-203).
fn prompt_confirm(message: &str, out: &mut dyn Write) -> bool {
    let _ = write!(out, "{message} [y/N] ");
    let _ = out.flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    let answer = answer.trim().to_lowercase();
    answer == "y" || answer == "yes"
}

/// `ResolvedSession` (main.ts:143-147).
enum ResolvedSession {
    Path(PathBuf),
    Local(PathBuf),
    Global { path: PathBuf, cwd: String },
    NotFound(String),
}

/// `findLocalSessionByExactId` (main.ts:153-161).
fn find_local_session_by_exact_id(
    session_id: &str,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Option<PathBuf> {
    SessionManager::list(cwd, session_dir)
        .into_iter()
        .find(|s| s.id == session_id)
        .map(|s| s.path)
}

/// `resolveSessionPath` (main.ts:163-189).
fn resolve_session_path(
    session_arg: &str,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> ResolvedSession {
    // If it looks like a file path, resolve it directly.
    if session_arg.contains('/') || session_arg.contains('\\') || session_arg.ends_with(".jsonl") {
        return ResolvedSession::Path(resolve_path(session_arg, cwd));
    }

    // Current project: exact id, then id prefix.
    let local_sessions = SessionManager::list(cwd, session_dir);
    let local_match = local_sessions
        .iter()
        .find(|s| s.id == session_arg)
        .or_else(|| {
            local_sessions
                .iter()
                .find(|s| s.id.starts_with(session_arg))
        });
    if let Some(local_match) = local_match {
        return ResolvedSession::Local(local_match.path.clone());
    }

    // Global search across all projects.
    let all_sessions = SessionManager::list_all(session_dir);
    let global_match = all_sessions
        .iter()
        .find(|s| s.id == session_arg)
        .or_else(|| all_sessions.iter().find(|s| s.id.starts_with(session_arg)));
    if let Some(global_match) = global_match {
        return ResolvedSession::Global {
            path: global_match.path.clone(),
            cwd: global_match.cwd.clone(),
        };
    }

    ResolvedSession::NotFound(session_arg.to_owned())
}

/// `validateForkFlags` (main.ts:205-219).
fn validate_fork_flags(parsed: &Args) -> Result<(), String> {
    if parsed.fork.is_none() {
        return Ok(());
    }
    let mut conflicting: Vec<&str> = Vec::new();
    if parsed.session.is_some() {
        conflicting.push("--session");
    }
    if parsed.continue_ {
        conflicting.push("--continue");
    }
    if parsed.resume {
        conflicting.push("--resume");
    }
    if parsed.no_session {
        conflicting.push("--no-session");
    }
    if conflicting.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "--fork cannot be combined with {}",
            conflicting.join(", ")
        ))
    }
}

/// `validateSessionIdFlags` (main.ts:221-242).
fn validate_session_id_flags(parsed: &Args) -> Result<(), String> {
    let Some(session_id) = &parsed.session_id else {
        return Ok(());
    };
    let mut conflicting: Vec<&str> = Vec::new();
    if parsed.session.is_some() {
        conflicting.push("--session");
    }
    if parsed.continue_ {
        conflicting.push("--continue");
    }
    if parsed.resume {
        conflicting.push("--resume");
    }
    if !conflicting.is_empty() {
        return Err(format!(
            "--session-id cannot be combined with {}",
            conflicting.join(", ")
        ));
    }
    assert_valid_session_id(session_id).map_err(|error| match error {
        // User-facing message without the `RpiError` Display prefix
        // (main.ts:237-240 prints `error.message` raw).
        RpiError::Session(message) => message,
        other => other.to_string(),
    })
}

/// `createSessionManager` (main.ts:264-355).
async fn create_session_manager(
    parsed: &Args,
    cwd: &Path,
    session_dir: Option<&Path>,
    startup_settings_manager: &SettingsManager,
    err: &mut dyn Write,
) -> Result<SessionManager, String> {
    if parsed.no_session || parsed.help || parsed.list_models.is_some() {
        return SessionManager::in_memory(
            Some(cwd),
            NewSessionOptions {
                id: parsed.session_id.clone(),
                parent_session: None,
            },
        )
        .map_err(|e| e.to_string());
    }

    if let Some(fork_arg) = &parsed.fork {
        if let Some(session_id) = &parsed.session_id {
            if find_local_session_by_exact_id(session_id, cwd, session_dir).is_some() {
                return Err(format!("Session already exists with id '{session_id}'"));
            }
        }

        return match resolve_session_path(fork_arg, cwd, session_dir) {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                SessionManager::fork_from(
                    &path,
                    cwd,
                    session_dir,
                    crate::core::session_manager::ForkOptions {
                        id: parsed.session_id.clone(),
                        entry_id: None,
                        position: None,
                    },
                )
                .map_err(|e| e.to_string())
            }
            ResolvedSession::Global { path, .. } => SessionManager::fork_from(
                &path,
                cwd,
                session_dir,
                crate::core::session_manager::ForkOptions {
                    id: parsed.session_id.clone(),
                    entry_id: None,
                    position: None,
                },
            )
            .map_err(|e| e.to_string()),
            ResolvedSession::NotFound(arg) => Err(format!("No session found matching '{arg}'")),
        };
    }

    if let Some(session_arg) = &parsed.session {
        return match resolve_session_path(session_arg, cwd, session_dir) {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                SessionManager::open(&path, session_dir, None).map_err(|e| e.to_string())
            }
            ResolvedSession::Global {
                path,
                cwd: other_cwd,
            } => {
                // Headless stdout is reserved for payload (upstream routes
                // everything through takeOverStdout → stderr).
                let _ = writeln!(err, "Session found in different project: {other_cwd}");
                if !prompt_confirm("Fork this session into current directory?", err) {
                    let _ = writeln!(err, "Aborted.");
                    std::process::exit(0);
                }
                SessionManager::fork_from(
                    &path,
                    cwd,
                    session_dir,
                    crate::core::session_manager::ForkOptions {
                        id: None,
                        entry_id: None,
                        position: None,
                    },
                )
                .map_err(|e| e.to_string())
            }
            ResolvedSession::NotFound(arg) => Err(format!("No session found matching '{arg}'")),
        };
    }

    if parsed.resume {
        // `selectSession` (main.ts:321-333, cli/session-picker.ts): a
        // standalone startup TUI picker shown before the session manager is
        // created — in every mode, not just interactive.
        let selected =
            crate::cli::session_picker::select_session(cwd, session_dir, startup_settings_manager)
                .await
                .map_err(|e| e.to_string())?;
        let Some(selected_path) = selected else {
            // Cancelled (main.ts:328-330): stdout message, exit code 0.
            println!("No session selected");
            std::process::exit(0);
        };
        return SessionManager::open(Path::new(&selected_path), session_dir, None)
            .map_err(|e| e.to_string());
    }

    if parsed.continue_ {
        return SessionManager::continue_recent(cwd, session_dir).map_err(|e| e.to_string());
    }

    if let Some(session_id) = &parsed.session_id {
        if let Some(existing) = find_local_session_by_exact_id(session_id, cwd, session_dir) {
            return SessionManager::open(&existing, session_dir, None).map_err(|e| e.to_string());
        }
        let _ = writeln!(
            err,
            "Warning: No project session found with id '{session_id}'; creating a new session with that id."
        );
    }

    let dir = match session_dir {
        Some(dir) => dir.to_path_buf(),
        None => crate::config::get_default_session_dir(cwd).map_err(|e| e.to_string())?,
    };
    SessionManager::create(
        cwd,
        Some(&dir),
        NewSessionOptions {
            id: parsed.session_id.clone(),
            parent_session: None,
        },
    )
    .map_err(|e| e.to_string())
}

/// `resolveCliPaths` (main.ts:455-457).
fn resolve_cli_paths(cwd: &Path, paths: &Option<Vec<String>>) -> Option<Vec<String>> {
    paths.as_ref().map(|paths| {
        paths
            .iter()
            .map(|value| {
                if is_local_path(value) {
                    resolve_path(value, cwd).to_string_lossy().into_owned()
                } else {
                    value.clone()
                }
            })
            .collect()
    })
}

/// `isLocalPath` (utils/paths.ts:41-56): bare names, relative paths and
/// `file:` URLs are local; package sources and remote URLs are not.
fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:"))
}

/// `buildSessionOptions` (main.ts:357-453).
struct SessionOptionsOut {
    model: Option<rpi_ai::types::Model>,
    thinking_level: Option<ThinkingLevel>,
    scoped_models: Vec<ScopedModel>,
    tools: Option<Vec<String>>,
    exclude_tools: Option<Vec<String>>,
    no_tools: Option<NoTools>,
    cli_thinking_from_model: bool,
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

fn build_session_options(
    parsed: &Args,
    scoped_models: &[ScopedModel],
    has_existing_session: bool,
    model_runtime: &ModelRuntime,
    settings_manager: &SettingsManager,
) -> SessionOptionsOut {
    let mut out = SessionOptionsOut {
        model: None,
        thinking_level: None,
        scoped_models: Vec::new(),
        tools: None,
        exclude_tools: None,
        no_tools: None,
        cli_thinking_from_model: false,
        diagnostics: Vec::new(),
    };

    // Model from CLI (main.ts:372-397).
    if let Some(cli_model) = &parsed.model {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: parsed.provider.as_deref(),
            cli_model: Some(cli_model),
            cli_thinking: parsed.thinking,
            model_runtime,
        });
        if let Some(warning) = resolved.warning {
            out.diagnostics.push(AgentSessionRuntimeDiagnostic {
                level: DiagnosticLevel::Warning,
                message: warning,
            });
        }
        if let Some(error) = resolved.error {
            out.diagnostics.push(AgentSessionRuntimeDiagnostic {
                level: DiagnosticLevel::Error,
                message: error,
            });
        }
        if let Some(model) = resolved.model {
            // `--model <pattern>:<thinking>` shorthand; explicit --thinking
            // still takes precedence (applied below).
            if parsed.thinking.is_none() && resolved.thinking_level.is_some() {
                out.thinking_level = resolved.thinking_level;
                out.cli_thinking_from_model = true;
            }
            out.model = Some(model);
        }
    }

    if out.model.is_none() && !scoped_models.is_empty() && !has_existing_session {
        // Saved default wins when it is inside the scope (main.ts:399-419).
        let saved_model = match (
            settings_manager.get_default_provider(),
            settings_manager.get_default_model(),
        ) {
            (Some(provider), Some(model_id)) => model_runtime.get_model(&provider, &model_id),
            _ => None,
        };
        let saved_in_scope = saved_model.and_then(|saved| {
            scoped_models
                .iter()
                .find(|sm| rpi_ai::models::models_are_equal(Some(&sm.model), Some(&saved)))
        });
        match saved_in_scope {
            Some(scoped) => {
                out.model = Some(scoped.model.clone());
                if parsed.thinking.is_none() && scoped.thinking_level.is_some() {
                    out.thinking_level = scoped.thinking_level;
                }
            }
            None => {
                let first = &scoped_models[0];
                out.model = Some(first.model.clone());
                if parsed.thinking.is_none() && first.thinking_level.is_some() {
                    out.thinking_level = first.thinking_level;
                }
            }
        }
    }

    // Explicit --thinking takes precedence (main.ts:421-424).
    if parsed.thinking.is_some() {
        out.thinking_level = parsed.thinking;
    }

    // Scoped models for cycling (main.ts:426-434).
    if !scoped_models.is_empty() {
        out.scoped_models = scoped_models.to_vec();
    }

    // Tools (main.ts:439-450).
    if parsed.no_tools {
        out.no_tools = Some(NoTools::All);
    } else if parsed.no_builtin_tools {
        out.no_tools = Some(NoTools::Builtin);
    }
    out.tools = parsed.tools.clone();
    out.exclude_tools = parsed.exclude_tools.clone();

    out
}

/// Prepare the initial message (main.ts:121-140).
async fn prepare_initial_message(
    parsed: &mut Args,
    auto_resize_images: bool,
    stdin_content: Option<&str>,
) -> Result<(Option<String>, Option<Vec<rpi_ai::types::ImageContent>>), FileProcessError> {
    if parsed.file_args.is_empty() {
        let result = build_initial_message(&mut parsed.messages, None, None, stdin_content);
        return Ok((result.initial_message, result.initial_images));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let processed = process_file_arguments(&parsed.file_args, &cwd, auto_resize_images).await?;
    let result = build_initial_message(
        &mut parsed.messages,
        Some(&processed.text),
        Some(processed.images),
        stdin_content,
    );
    Ok((result.initial_message, result.initial_images))
}

/// `main` (main.ts:473-864). Returns the process exit code.
pub async fn run_app(args: Vec<String>) -> i32 {
    // Note: `Stdout`/`Stderr` values lock internally per write. Holding a
    // process-global `StdoutLock`/`StderrLock` across `.await` deadlocks any
    // spawned task that writes to stdout/stderr (RPC writer/command tasks).
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();

    // Offline mode (main.ts:476-480).
    let offline_mode = args.iter().any(|a| a == "--offline")
        || is_truthy_env_flag(std::env::var(ENV_OFFLINE).ok().as_deref());
    if offline_mode {
        // SAFETY-FREE note: single-threaded startup phase; no readers race.
        std::env::set_var(ENV_OFFLINE, "1");
        std::env::set_var(ENV_SKIP_VERSION_CHECK, "1");
    }

    // Subcommand dispatch (main.ts:492-507). `update --models` landed in
    // W6-C (remote model catalog refresh); install/remove/uninstall/list
    // landed in T14-W2; the remaining update targets and `config` landed
    // in T14-W3.
    if let Some(first) = args.first() {
        if first == "config" {
            return crate::cli::config_command::run_config(&args).await;
        }
        if first == "update" {
            return crate::cli::package_command::run_update(&args).await;
        }
        if crate::cli::package_command::parse_package_command(&args).is_some() {
            return crate::cli::package_command::run_package_command(&args);
        }
    }

    let parsed = parse_args(&args);
    if !parsed.diagnostics.is_empty() {
        for diagnostic in &parsed.diagnostics {
            let prefix = match diagnostic.level {
                DiagnosticLevel::Error => "Error",
                _ => "Warning",
            };
            let _ = writeln!(err, "{prefix}: {diagnostic}");
        }
        if has_error(&parsed.diagnostics) {
            return 1;
        }
    }

    if parsed.version {
        let _ = writeln!(out, "{VERSION}");
        return 0;
    }

    if let Some(export_file) = &parsed.export {
        // `rpi --export <file> [output.html]` (main.ts:526-538): export the
        // session file to HTML and exit; the first positional message is the
        // output path.
        let output_path = parsed.messages.first().map(String::as_str);
        let options = crate::core::export_html::ExportOptions::from_output_path(output_path);
        return match crate::core::export_html::export_from_file(export_file, &options) {
            Ok(result) => {
                let _ = writeln!(out, "Exported to: {result}");
                0
            }
            Err(error) => {
                let _ = writeln!(err, "Error: {}", error.raw_message());
                1
            }
        };
    }

    let mut app_mode = resolve_app_mode(
        &parsed,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    // Tracing init (coding-standards §16): stderr sink for print/json/rpc,
    // file sink for the interactive TUI (never stderr while rendering).
    // Must run before any `tracing::` call; the handle lives until run_app
    // returns so the buffered file writer is flushed on drop.
    let _log_sink = crate::logging::LogSink::install(app_mode == AppMode::Interactive);

    if parsed.mode == Some(Mode::Rpc) && !parsed.file_args.is_empty() {
        let _ = writeln!(err, "Error: @file arguments are not supported in RPC mode");
        return 1;
    }

    for validation in [
        validate_fork_flags(&parsed),
        validate_session_id_flags(&parsed),
    ] {
        if let Err(message) = validation {
            let _ = writeln!(err, "Error: {message}");
            return 1;
        }
    }

    // Migrations are permanently out of scope (ADR-0003 §3).

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let agent_dir = get_agent_dir();
    let mut startup_settings_manager = SettingsManager::create(
        &cwd,
        Some(&agent_dir),
        SettingsManagerCreateOptions::default(),
    );
    report_diagnostics(
        &collect_settings_diagnostics(&mut startup_settings_manager, "startup session lookup"),
        &mut err,
    );

    // First-time setup is interactive-only (T12); headless modes skip it.

    let settings_session_dir = startup_settings_manager.get_session_dir();
    let session_dir = resolve_session_dir_from_env(
        parsed.session_dir.as_deref(),
        settings_session_dir.as_deref(),
    );
    let mut session_manager = match create_session_manager(
        &parsed,
        &cwd,
        session_dir.as_deref(),
        &startup_settings_manager,
        &mut err,
    )
    .await
    {
        Ok(manager) => manager,
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            return 1;
        }
    };

    // Header cwd missing (main.ts:579-591): interactive prompts (T12);
    // headless errors out.
    if let Some(issue) = get_missing_session_cwd_issue(&session_manager, &cwd) {
        if app_mode == AppMode::Interactive {
            // T12 prompt path; unreachable until interactive lands.
            let _ = writeln!(err, "{}", format_missing_session_cwd_error(&issue));
            return 1;
        }
        let _ = writeln!(err, "{}", format_missing_session_cwd_error(&issue));
        return 1;
    }

    if let Some(name) = &parsed.name {
        let name = name.trim();
        if name.is_empty() {
            let _ = writeln!(err, "Error: --name requires a non-empty value");
            return 1;
        }
        if let Err(error) = session_manager.append_session_info(name) {
            let _ = writeln!(err, "Error: {error}");
            return 1;
        }
    }

    let parsed = Arc::new(parsed);
    let trust_store = Arc::new(ProjectTrustStore::new(&agent_dir));
    let trust_by_cwd: Arc<Mutex<HashMap<PathBuf, bool>>> = Arc::new(Mutex::new(HashMap::new()));
    let default_project_trust =
        default_project_trust_from_settings(startup_settings_manager.get_default_project_trust());
    // Startup trust prompt settings (main.ts:650-653 + startup-ui.ts:60-68):
    // an in-memory manager over the global settings — the prompt runs before
    // project trust resolves, so project settings must not leak in.
    let trust_prompt_settings = Arc::new(SettingsManager::in_memory(
        startup_settings_manager.get_global_settings(),
        SettingsManagerCreateOptions::default(),
    ));
    // `trustPromptMode` (main.ts:608): help/--list-models run the prompt
    // path in print mode, i.e. without UI.
    let trust_prompt_has_ui =
        app_mode == AppMode::Interactive && !parsed.help && parsed.list_models.is_none();
    let resolved_extension_paths = resolve_cli_paths(&cwd, &parsed.extensions);
    let resolved_skill_paths = resolve_cli_paths(&cwd, &parsed.skills);
    let resolved_prompt_template_paths = resolve_cli_paths(&cwd, &parsed.prompt_templates);
    let resolved_theme_paths = resolve_cli_paths(&cwd, &parsed.themes);

    /// T15 W7: resolve package resources (extensions/skills/prompts/themes)
    /// per the current trust state; failures degrade to empty + a warning
    /// (the resource loader surfaces discovery diagnostics as usual).
    async fn resolve_package_resource_paths(
        cwd: &Path,
        agent_dir: &Path,
        project_trusted: bool,
    ) -> crate::core::resource_loader::PackageResourcePaths {
        // Fresh settings manager per call (SettingsManager is not Clone;
        // the package manager takes it by value).
        let settings_manager = SettingsManager::create(
            cwd,
            Some(agent_dir),
            SettingsManagerCreateOptions { project_trusted },
        );
        // `packageManager.resolve()` — the package slice
        // (resource-loader.ts:495 `this.packageManager.resolve()`).
        match crate::core::package_manager::DefaultPackageManager::with_options(
            crate::core::package_manager::PackageManagerOptions {
                cwd: cwd.to_path_buf(),
                agent_dir: agent_dir.to_path_buf(),
                settings_manager,
                runner: None,
                offline: None,
            },
        )
        .resolve(None)
        {
            Ok(paths) => paths.to_package_resource_paths(),
            Err(message) => {
                eprintln!("Warning: package resolution failed: {message}");
                crate::core::resource_loader::PackageResourcePaths::default()
            }
        }
    }

    fn enabled_extension_paths(
        paths: &crate::core::resource_loader::PackageResourcePaths,
    ) -> Vec<String> {
        paths
            .extension_paths
            .iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.path.to_string_lossy().into_owned())
            .collect()
    }

    // `createRuntime` factory (main.ts:615-739).
    // T15 W3: the extension host built inside the closure is stashed here so
    // `--help` can render the dynamic extension-flag section (main.ts:752-758).
    let startup_host: Arc<Mutex<Option<Arc<rpi_ext_host::host::NativeExtensionHost>>>> =
        Arc::new(Mutex::new(None));
    let create_runtime = {
        let parsed = parsed.clone();
        let trust_store = trust_store.clone();
        let startup_host = startup_host.clone();
        let trust_by_cwd = trust_by_cwd.clone();
        Arc::new(move |options: CreateRuntimeOptions| {
            let parsed = parsed.clone();
            let trust_store = trust_store.clone();
            let trust_by_cwd = trust_by_cwd.clone();
            let startup_host = startup_host.clone();
            let trust_prompt_settings = trust_prompt_settings.clone();
            let resolved_extension_paths = resolved_extension_paths.clone();
            let resolved_skill_paths = resolved_skill_paths.clone();
            let resolved_prompt_template_paths = resolved_prompt_template_paths.clone();
            let resolved_theme_paths = resolved_theme_paths.clone();
            Box::pin(async move {
                // `isInitialRuntime` (main.ts:622): the trust prompt with UI
                // is limited to the initial runtime (main.ts:654).
                let is_initial_runtime = options.session_start_event.is_none();
                let cwd = options.cwd.clone();
                let has_trust_requiring = has_trust_requiring_project_resources(&cwd);
                let cached = trust_by_cwd
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&cwd)
                    .copied();
                let should_resolve_trust = parsed.project_trust_override.is_none()
                    && cached.is_none()
                    && has_trust_requiring;
                let project_trusted = if should_resolve_trust {
                    false
                } else {
                    let stored_trust = trust_store.get(&cwd)?;
                    cached
                        .or(parsed.project_trust_override)
                        .unwrap_or(!has_trust_requiring || stored_trust == Some(true))
                };

                let runtime_settings_manager = SettingsManager::create(
                    &cwd,
                    Some(&options.agent_dir),
                    SettingsManagerCreateOptions { project_trusted },
                );
                // T15 W7: package resources feed the loader's rank-4 port
                // (skills/prompts/themes) and the extension host's package
                // paths (merge order: CLI first, packages after).
                let package_resource_paths =
                    resolve_package_resource_paths(&cwd, &options.agent_dir, project_trusted).await;
                let services = create_agent_session_services(CreateAgentSessionServicesOptions {
                    cwd: cwd.clone(),
                    agent_dir: Some(options.agent_dir.clone()),
                    settings_manager: Some(runtime_settings_manager),
                    model_runtime: None,
                    extension_flag_values: parsed.unknown_flags.clone(),
                    resource_loader_options: Some({
                        let parsed = parsed.clone();
                        Box::new(move |loader_options| {
                            loader_options.additional_extension_paths =
                                resolved_extension_paths.unwrap_or_default();
                            loader_options.additional_skill_paths =
                                resolved_skill_paths.unwrap_or_default();
                            loader_options.additional_prompt_template_paths =
                                resolved_prompt_template_paths.unwrap_or_default();
                            loader_options.additional_theme_paths =
                                resolved_theme_paths.unwrap_or_default();
                            loader_options.no_extensions = parsed.no_extensions;
                            loader_options.no_skills = parsed.no_skills;
                            loader_options.no_prompt_templates = parsed.no_prompt_templates;
                            loader_options.no_themes = parsed.no_themes;
                            loader_options.no_context_files = parsed.no_context_files;
                            loader_options.system_prompt = parsed.system_prompt.clone();
                            loader_options.append_system_prompt =
                                parsed.append_system_prompt.clone();
                        })
                    }),
                })
                .await?;

                // T15 W2: the real extension host enters the startup
                // pipeline with the two-phase trust-aware load
                // (resource-loader.ts:327-353 `loadProjectTrustExtensions`,
                // :520-571 `loadFinalExtensionSet`). CLI `-e` paths are the
                // existence-checked list from the resource loader.
                let cli_extension_paths: Vec<String> = {
                    let loader = services
                        .resource_loader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    loader
                        .resources()
                        .extensions
                        .paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect()
                };
                let extension_host =
                    rpi_ext_host::host::NativeExtensionHost::new(&cwd.to_string_lossy());
                // Built-in hidden extensions (extensions/index.ts
                // `builtInExtensions`) load as inline factories — trust
                // independent, like upstream.
                let builtin_extensions = vec![crate::extensions::llama::inline_extension()];
                let mut project_trust_diagnostics: Vec<AgentSessionRuntimeDiagnostic> = Vec::new();

                // Two-phase trust resolution (main.ts:626-662 +
                // resourceLoaderReloadOptions.resolveProjectTrust).
                let extension_load_errors = if should_resolve_trust {
                    // Pre-trust bootstrap: user/global + CLI `-e` + inline;
                    // project-local extensions stay out (its errors are
                    // carried into the final pass result, so drop them here).
                    let _ = extension_host
                        .load_startup_pre_trust(
                            options.agent_dir.clone(),
                            cli_extension_paths.clone(),
                            builtin_extensions.clone(),
                            parsed.no_extensions,
                        )
                        .await;

                    // The `project_trust` extension event on the pre-trust
                    // set (project-trust.ts:54-70); its result takes the
                    // priority position of `resolve_project_trusted`'s
                    // `extension_event` parameter.
                    let (trust_result, trust_errors) = extension_host
                        .emit_project_trust(serde_json::json!({
                            "type": "project_trust",
                            "cwd": cwd.to_string_lossy(),
                        }))
                        .await;
                    for error in trust_errors {
                        project_trust_diagnostics.push(AgentSessionRuntimeDiagnostic {
                            level: DiagnosticLevel::Warning,
                            message: format!(
                                "Extension \"{}\" project_trust error: {}",
                                error.extension_path, error.error
                            ),
                        });
                    }
                    let extension_event = trust_result
                        .as_ref()
                        .map(crate::core::extension_host_adapter::parse_project_trust_result);

                    let mut trust_context = match options.project_trust_context {
                        Some(context) => context,
                        None if is_initial_runtime && trust_prompt_has_ui => {
                            // `createProjectTrustContext` (cli/project-trust.ts):
                            // the startup selector runs before the TUI exists.
                            // NOTE: the selector blocks this executor thread
                            // while the user decides (startup is sequential).
                            let settings = trust_prompt_settings.clone();
                            ProjectTrustContext {
                                has_ui: true,
                                select_async: None,
                                select: Some(Box::new(move |title, options| {
                                    crate::modes::interactive::startup_ui::run_startup_selector(
                                        &settings, title, options,
                                    )
                                })),
                            }
                        }
                        None => ProjectTrustContext::headless(),
                    };
                    // T15 W7 (ADR-0006): the async TUI selector takes
                    // precedence when the context carries one.
                    let trusted = if trust_context.select_async.is_some() {
                        crate::core::trust_manager::resolve_project_trusted_async(
                            &cwd,
                            &trust_store,
                            parsed.project_trust_override,
                            default_project_trust,
                            extension_event,
                            &mut trust_context,
                        )
                        .await?
                    } else {
                        resolve_project_trusted(
                            &cwd,
                            &trust_store,
                            parsed.project_trust_override,
                            default_project_trust,
                            extension_event,
                            &mut trust_context,
                        )?
                    };
                    trust_by_cwd
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(cwd.clone(), trusted);
                    let package_extension_paths = if trusted {
                        {
                            let mut loader = services
                                .resource_loader
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            loader.settings_manager_mut().set_project_trusted(true);
                            loader.set_project_trusted(true);
                        }
                        // Re-resolve packages with the trusted settings and
                        // re-feed the loader before the reload (T15 W7).
                        let trusted_paths =
                            resolve_package_resource_paths(&cwd, &options.agent_dir, true).await;
                        let extension_paths = enabled_extension_paths(&trusted_paths);
                        {
                            let mut loader = services
                                .resource_loader
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            loader.set_package_resources(trusted_paths);
                            loader.reload();
                        }
                        extension_paths
                    } else {
                        enabled_extension_paths(&package_resource_paths)
                    };

                    // Final pass: the full set for the resolved trust state;
                    // pre-trust extensions are reused (resource-loader.ts:520-571).
                    // The built-ins are re-passed so `/reload` replays them from
                    // the recorded spec — the reuse path does NOT re-run inline
                    // factories (pre-trust already loaded them).
                    extension_host
                        .load_startup_final(
                            options.agent_dir.clone(),
                            cli_extension_paths.clone(),
                            package_extension_paths,
                            builtin_extensions.clone(),
                            trusted,
                            parsed.no_extensions,
                        )
                        .await
                } else {
                    // Single-phase load (trust already decided or nothing to
                    // gate): project-local rides the current trust state;
                    // built-ins run here (no pre-trust pass).
                    extension_host
                        .load_startup_final(
                            options.agent_dir.clone(),
                            cli_extension_paths.clone(),
                            enabled_extension_paths(&package_resource_paths),
                            builtin_extensions.clone(),
                            project_trusted,
                            parsed.no_extensions,
                        )
                        .await
                };

                // Flush native provider registrations from extension load
                // into the model runtime BEFORE session creation, so
                // extension models are visible to initial model resolution
                // (upstream flushes at runner.bindCore; our bind happens
                // after session construction, so the native half goes
                // early — agent-session-services.ts:166-178).
                for registration in extension_host
                    .runtime()
                    .take_pending_native_provider_registrations()
                {
                    if let Err(message) = services
                        .model_runtime
                        .register_native_provider(registration.provider)
                        .await
                    {
                        project_trust_diagnostics.push(AgentSessionRuntimeDiagnostic {
                            level: DiagnosticLevel::Error,
                            message: format!(
                                "Extension \"{}\" error: {message}",
                                registration.extension_path
                            ),
                        });
                    }
                }

                // `applyExtensionFlagValues`
                // (agent-session-services.ts:81-127) — runs after the final
                // extension load so the registered flags are known.
                let flag_diagnostics =
                    crate::core::agent_session_services::apply_extension_flag_values(
                        &extension_host,
                        &parsed.unknown_flags,
                    );

                let mut diagnostics: Vec<AgentSessionRuntimeDiagnostic> = Vec::new();
                diagnostics.extend(project_trust_diagnostics);
                diagnostics.extend(services.diagnostics.clone());
                diagnostics.extend(flag_diagnostics);
                for error in &extension_load_errors {
                    diagnostics.push(AgentSessionRuntimeDiagnostic {
                        level: DiagnosticLevel::Error,
                        message: format!(
                            "Failed to load extension \"{}\": {}",
                            error.path, error.error
                        ),
                    });
                }
                {
                    let mut loader = services
                        .resource_loader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    diagnostics.extend(collect_settings_diagnostics(
                        loader.settings_manager_mut(),
                        "runtime creation",
                    ));
                    for error in &loader.resources().extensions.errors {
                        diagnostics.push(AgentSessionRuntimeDiagnostic {
                            level: DiagnosticLevel::Error,
                            message: format!(
                                "Failed to load extension \"{}\": {}",
                                error.path.display(),
                                error.error
                            ),
                        });
                    }
                }

                let model_patterns = parsed.models.clone().or_else(|| {
                    let loader = services
                        .resource_loader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    loader.settings_manager().get_enabled_models()
                });
                let scoped_models = match model_patterns {
                    Some(patterns) if !patterns.is_empty() => {
                        let result = resolve_model_scope_with_diagnostics(
                            &patterns,
                            &services.model_runtime,
                        )
                        .await;
                        // Upstream prints these immediately via console.warn
                        // (model-resolver.ts:355-361).
                        for diagnostic in &result.diagnostics {
                            eprintln!("Warning: {}", diagnostic.message);
                        }
                        result.scoped_models
                    }
                    _ => Vec::new(),
                };

                let has_existing_session = !options
                    .session_manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .build_session_context()
                    .messages
                    .is_empty();
                let session_options = {
                    let loader = services
                        .resource_loader
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    build_session_options(
                        &parsed,
                        &scoped_models,
                        has_existing_session,
                        &services.model_runtime,
                        loader.settings_manager(),
                    )
                };
                diagnostics.extend(session_options.diagnostics);

                // --api-key: non-persistent runtime override
                // (main.ts:705-715).
                if let Some(api_key) = &parsed.api_key {
                    match &session_options.model {
                        None => diagnostics.push(AgentSessionRuntimeDiagnostic {
                            level: DiagnosticLevel::Error,
                            message: "--api-key requires a model to be specified via --model, --provider/--model, or --models".to_owned(),
                        }),
                        Some(model) => {
                            services
                                .model_runtime
                                .set_runtime_api_key(&model.provider, api_key)
                                .await;
                            let _ = services.model_runtime.get_available(None).await;
                        }
                    }
                }

                let services_for_options = services.clone();
                let extension_host = Arc::new(extension_host);
                *startup_host.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(extension_host.clone());
                let created = create_agent_session(CreateAgentSessionOptions {
                    cwd: Some(cwd.clone()),
                    agent_dir: Some(options.agent_dir.clone()),
                    model_runtime: Some(services.model_runtime.clone()),
                    model: session_options.model,
                    thinking_level: session_options.thinking_level,
                    scoped_models: session_options.scoped_models,
                    no_tools: session_options.no_tools,
                    tools: session_options.tools,
                    exclude_tools: session_options.exclude_tools,
                    custom_tools: Vec::new(),
                    services: Some(services_for_options),
                    session_manager: Some(options.session_manager),
                    extension_host: Some(extension_host.clone()),
                    session_start_event: options.session_start_event.or(Some(SessionStartEvent {
                        reason: SessionStartReason::Startup,
                        previous_session_file: None,
                    })),
                })
                .await?;

                // `runner.bindCore(...)` (agent-session.ts:2356-2443): the
                // host actions bind against the live session; queued
                // provider registrations flush here.
                crate::core::extension_actions::bind_session_actions(
                    &extension_host,
                    &created.session,
                )
                .await;

                // CLI thinking override (main.ts:729-732).
                let cli_thinking_override =
                    parsed.thinking.is_some() || session_options.cli_thinking_from_model;
                if created.session.model().is_some() && cli_thinking_override {
                    let level = created.session.thinking_level();
                    created.session.set_thinking_level(level);
                }

                Ok(CreateAgentSessionRuntimeResult {
                    session: created.session,
                    services: created.services.unwrap_or(services),
                    diagnostics,
                    model_fallback_message: created.model_fallback_message,
                })
            })
                as futures::future::BoxFuture<
                    'static,
                    Result<CreateAgentSessionRuntimeResult, RpiError>,
                >
        })
    };

    let runtime_result = create_agent_session_runtime(
        create_runtime,
        CreateRuntimeOptions {
            cwd: session_manager.get_cwd().to_path_buf(),
            agent_dir: agent_dir.clone(),
            session_manager: Arc::new(Mutex::new(session_manager)),
            session_start_event: None,
            project_trust_context: None,
        },
    )
    .await;
    let mut runtime = match runtime_result {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "Error: {error}");
            return 1;
        }
    };

    if parsed.help {
        // Dynamic extension-flag section (main.ts:752-758), populated from
        // the host the initial runtime loaded.
        let extension_flags: Vec<crate::cli::args::ExtensionFlag> = startup_host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|host| {
                host.get_flags()
                    .iter()
                    .map(|(_, flag)| crate::cli::args::ExtensionFlag {
                        name: flag.name.clone(),
                        flag_type: match flag.flag_type {
                            rpi_ext_host::types::FlagType::Boolean => "boolean".to_owned(),
                            rpi_ext_host::types::FlagType::String => "string".to_owned(),
                        },
                        description: flag.description.clone(),
                        extension_path: flag.extension_path.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let _ = write!(
            out,
            "{}",
            print_help(&extension_flags, std::io::stdout().is_terminal())
        );
        return 0;
    }

    if let Some(list_models_flag) = &parsed.list_models {
        let search = match list_models_flag {
            crate::cli::args::ListModels::All => None,
            crate::cli::args::ListModels::Search(pattern) => Some(pattern.as_str()),
        };
        let (warning, text) = list_models(runtime.services().model_runtime.as_ref(), search).await;
        if let Some(warning) = warning {
            let _ = writeln!(err, "{warning}");
        }
        let _ = write!(out, "{text}");
        return 0;
    }

    // Read piped stdin (main.ts:766-774) — skipped for RPC.
    let mut stdin_content: Option<String> = None;
    if app_mode != AppMode::Rpc {
        stdin_content = read_piped_stdin();
        if stdin_content.is_some() && app_mode == AppMode::Interactive {
            app_mode = AppMode::Print;
        }
    }

    let mut parsed_owned = (*parsed).clone();
    let auto_resize = {
        let loader = runtime
            .services()
            .resource_loader
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        loader.settings_manager().get_image_auto_resize()
    };
    let (initial_message, initial_images) =
        match prepare_initial_message(&mut parsed_owned, auto_resize, stdin_content.as_deref())
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let _ = writeln!(err, "Error: {error}");
                return 1;
            }
        };

    report_diagnostics(runtime.diagnostics(), &mut err);
    if runtime
        .diagnostics()
        .iter()
        .any(|d| d.level == DiagnosticLevel::Error)
    {
        if runtime
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("Failed to load extension"))
        {
            let _ = writeln!(err, "{EXTENSION_LOAD_FAILURE_HINT}");
        }
        return 1;
    }

    if app_mode != AppMode::Interactive && runtime.session().model().is_none() {
        let _ = writeln!(err, "{}", format_no_models_available_message());
        return 1;
    }

    // Captured before the runtime is moved into the interactive mode
    // (T12-S4b; main.ts:883-885 modelFallbackMessage warning).
    let runtime_model_fallback_message = runtime.model_fallback_message().map(str::to_string);

    match app_mode {
        AppMode::Rpc => {
            let out: Box<dyn Write + Send> = Box::new(std::io::stdout());
            let stdin = tokio::io::BufReader::new(tokio::io::stdin());
            run_rpc_mode(runtime, stdin, out).await
        }
        AppMode::Interactive => {
            // Interactive mode (T12): the TUI owns the terminal; the runtime
            // is moved in and disposed on exit (interactive-mode.ts:832-920).
            run_interactive_mode(
                runtime,
                crate::modes::interactive::InteractiveModeOptions {
                    model_fallback_message: runtime_model_fallback_message,
                    initial_message,
                    initial_images,
                    initial_messages: parsed_owned.messages,
                    verbose: parsed_owned.verbose,
                },
            )
            .await
        }
        AppMode::Print | AppMode::Json => {
            let exit_code = run_print_mode(
                &mut runtime,
                PrintModeOptions {
                    mode: match app_mode {
                        AppMode::Json => PrintOutputMode::Json,
                        _ => PrintOutputMode::Text,
                    },
                    messages: parsed_owned.messages,
                    initial_message,
                    initial_images,
                },
                &mut out,
                &mut err,
            )
            .await;
            exit_code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_local_path;

    /// `isLocalPath` (utils/paths.ts:44-52): `github:` and bare `http:` /
    /// `https:` / `ssh:` prefixes are non-local too.
    #[test]
    fn test_is_local_path_prefixes() {
        for remote in [
            "npm:package",
            "git:repo",
            "github:user/repo",
            "http:example.com/x",
            "http://example.com/x",
            "https:example.com/x",
            "https://example.com/x",
            "ssh:git@example.com",
            "ssh://git@example.com",
        ] {
            assert!(!is_local_path(remote), "{remote} should be remote");
        }
        for local in [
            "skill-name",
            "./relative/path",
            "../up/path",
            "file:///abs/path",
            "/abs/path",
        ] {
            assert!(is_local_path(local), "{local} should be local");
        }
    }
}
