//! Single point of path resolution (coding-standards §10.1).
//!
//! Port of the path-related subset of `packages/coding-agent/src/config.ts`
//! (`getAgentDir` / `getSessionsDir` / env var names) and
//! `packages/coding-agent/src/core/session-manager.ts`
//! (`getDefaultSessionDirPath` / `getDefaultSessionDir`), plus the session-dir
//! override chain from `packages/coding-agent/src/main.ts:573-577`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences (ADR-0001, requirements §1.4):
//! - `APP_NAME` is `rpi`, `CONFIG_DIR_NAME` is `.rpi`, the env prefix is
//!   `RPI_` (upstream: `pi` / `.pi` / `PI_`).
//! - Home directory comes from `HOME` (unix) / `USERPROFILE` (windows)
//!   directly; there is no `os.homedir()` equivalent in the dependency
//!   baseline.

use std::path::{Path, PathBuf};

use crate::tools::path_utils::{normalize_path, resolve_path};

/// `APP_NAME` (config.ts:489) — Rpi rename (ADR-0001).
pub const APP_NAME: &str = "rpi";
/// `CONFIG_DIR_NAME` (config.ts:491) — Rpi rename (ADR-0001).
pub const CONFIG_DIR_NAME: &str = ".rpi";
/// `ENV_AGENT_DIR` = `{APP_NAME}_CODING_AGENT_DIR` (config.ts:495).
pub const ENV_AGENT_DIR: &str = "RPI_CODING_AGENT_DIR";
/// `ENV_SESSION_DIR` = `{APP_NAME}_CODING_AGENT_SESSION_DIR` (config.ts:496).
pub const ENV_SESSION_DIR: &str = "RPI_CODING_AGENT_SESSION_DIR";
/// `PI_OFFLINE` → `RPI_OFFLINE` (requirements §3.3).
pub const ENV_OFFLINE: &str = "RPI_OFFLINE";
/// `PI_SKIP_VERSION_CHECK` → `RPI_SKIP_VERSION_CHECK` (requirements §3.3).
pub const ENV_SKIP_VERSION_CHECK: &str = "RPI_SKIP_VERSION_CHECK";
/// `PI_SHARE_VIEWER_URL` → `RPI_SHARE_VIEWER_URL` (requirements §3.3) — the
/// only env read for the `/share` viewer endpoint; W6 endpoint
/// configurability hooks in at [`get_share_viewer_url`].
pub const ENV_SHARE_VIEWER_URL: &str = "RPI_SHARE_VIEWER_URL";
/// Package version (`VERSION` in config.ts — from package.json upstream).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => return Some(PathBuf::from(home)),
            _ => {}
        }
        // Upstream `process.env.HOME || homedir()` (T14 review m7): with
        // HOME unset, `os.homedir()` falls back to the passwd entry; the
        // `~/.agents/skills` exemption (trust_manager) then still computes
        // the real home instead of an empty path.
        unsafe {
            let uid = libc::getuid();
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut buf = vec![0u8; 4096];
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            let rc = libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            );
            if rc == 0 && !result.is_null() && !pwd.pw_dir.is_null() {
                use std::os::unix::ffi::OsStringExt;
                let dir = std::ffi::CStr::from_ptr(pwd.pw_dir).to_bytes();
                return Some(PathBuf::from(std::ffi::OsString::from_vec(dir.to_vec())));
            }
        }
        None
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Atomic file write: write to a unique temp file in the same directory,
/// then rename over the target (T14 review: trust.json/settings.json are
/// crash-consistency-sensitive stores; upstream's plain `writeFileSync` can
/// leave a truncated file on crash mid-write, which hard-fails the next
/// read. A same-directory rename keeps the visible behavior identical while
/// making the write atomic — no new lock semantics, no observable format
/// change).
pub fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut os = path.as_os_str().to_owned();
    os.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let tmp = PathBuf::from(os);
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `getAgentDir` (config.ts:515-521): `RPI_CODING_AGENT_DIR` env override,
/// else `~/.rpi/agent`.
pub fn get_agent_dir() -> PathBuf {
    if let Some(env_dir) = std::env::var_os(ENV_AGENT_DIR) {
        if !env_dir.is_empty() {
            return PathBuf::from(normalize_path(&env_dir.to_string_lossy()));
        }
    }
    match home_dir() {
        Some(home) => home.join(CONFIG_DIR_NAME).join("agent"),
        None => PathBuf::from(CONFIG_DIR_NAME).join("agent"),
    }
}

/// `getSessionsDir` (config.ts:559-561).
pub fn get_sessions_dir() -> PathBuf {
    get_agent_dir().join("sessions")
}

/// `expandTildePath` (config.ts:498-500).
pub fn expand_tilde_path(path: &str) -> PathBuf {
    PathBuf::from(normalize_path(path))
}

/// `DEFAULT_SHARE_VIEWER_URL` (config.ts:502).
pub const DEFAULT_SHARE_VIEWER_URL: &str = "https://resetpi.com/session/";

/// `getShareViewerUrl` (config.ts:505-508): `{base}#{gistId}` with the
/// `RPI_SHARE_VIEWER_URL` override (empty falls back to the default, like the
/// upstream `||`). This is the single read of that env var.
pub fn get_share_viewer_url(gist_id: &str) -> String {
    let base_url = std::env::var(ENV_SHARE_VIEWER_URL)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SHARE_VIEWER_URL.to_string());
    format!("{base_url}#{gist_id}")
}

/// Encode a resolved cwd into the session subdirectory name
/// (`--<cwd>--` rule, `getDefaultSessionDirPath` at session-manager.ts:476-481):
/// strip one leading `/` or `\`, then replace every `/`, `\`, `:` with `-`.
pub fn encode_cwd_dir_name(resolved_cwd: &str) -> String {
    let stripped = resolved_cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(resolved_cwd);
    let safe: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            _ => c,
        })
        .collect();
    format!("--{safe}--")
}

/// `getDefaultSessionDirPath` (session-manager.ts:476-481).
///
/// Pure path computation — does not touch the filesystem. `agent_dir`
/// defaults to [`get_agent_dir`].
pub fn get_default_session_dir_path(cwd: &Path, agent_dir: Option<&Path>) -> PathBuf {
    let resolved_cwd = resolve_path(
        &cwd.to_string_lossy(),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    );
    let agent_dir = match agent_dir {
        Some(dir) => resolve_path(
            &dir.to_string_lossy(),
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        ),
        None => get_agent_dir(),
    };
    agent_dir
        .join("sessions")
        .join(encode_cwd_dir_name(&resolved_cwd.to_string_lossy()))
}

/// `getDefaultSessionDir` (session-manager.ts:483-489): creates the directory
/// recursively when missing.
pub fn get_default_session_dir(cwd: &Path) -> std::io::Result<PathBuf> {
    let session_dir = get_default_session_dir_path(cwd, None);
    if !session_dir.exists() {
        std::fs::create_dir_all(&session_dir)?;
    }
    Ok(session_dir)
}

/// Resolve the session storage directory from the override chain
/// (main.ts:573-577): `--session-dir` flag > `RPI_CODING_AGENT_SESSION_DIR`
/// env > `settings.sessionDir` > `None` (caller falls back to
/// [`get_default_session_dir`]).
///
/// Inputs are the raw, un-normalized values; each present level is normalized
/// (`expandTildePath` / `normalizePath` upstream). Empty strings are falsy
/// upstream (`parsed.sessionDir ? … : undefined`, main.ts:573-576) and fall
/// through to the next level here too. `settings_session_dir` is passed in
/// explicitly because settings parsing lands in T09.
pub fn resolve_session_dir(
    flag_session_dir: Option<&str>,
    env_session_dir: Option<&str>,
    settings_session_dir: Option<&str>,
) -> Option<PathBuf> {
    fn non_empty(level: Option<&str>) -> Option<&str> {
        level.filter(|s| !s.is_empty())
    }
    non_empty(flag_session_dir)
        .map(normalize_path)
        .or_else(|| non_empty(env_session_dir).map(normalize_path))
        .or_else(|| non_empty(settings_session_dir).map(normalize_path))
        .map(PathBuf::from)
}

/// Convenience wrapper over [`resolve_session_dir`] that reads
/// `RPI_CODING_AGENT_SESSION_DIR` from the process environment.
pub fn resolve_session_dir_from_env(
    flag_session_dir: Option<&str>,
    settings_session_dir: Option<&str>,
) -> Option<PathBuf> {
    let env = std::env::var(ENV_SESSION_DIR).ok();
    resolve_session_dir(flag_session_dir, env.as_deref(), settings_session_dir)
}

// ===== T09: settings & declarative-resource paths =====
//
// All resource discovery paths used by `core::settings_manager` and
// `core::resource_loader` live here (coding-standards §10.1). Upstream
// references: `settings-manager.ts:195-196`, `resource-loader.ts`,
// `package-manager.ts` (`addAutoDiscoveredResources`).

/// `SYSTEM.md` project/global system-prompt override file name
/// (resource-loader.ts).
pub const SYSTEM_PROMPT_FILE_NAME: &str = "SYSTEM.md";
/// `APPEND_SYSTEM.md` append variant file name (resource-loader.ts).
pub const APPEND_SYSTEM_PROMPT_FILE_NAME: &str = "APPEND_SYSTEM.md";
/// `.agents` directory name for the Agent Skills standard locations
/// (`~/.agents/skills` and ancestor `.agents/skills`).
pub const AGENTS_DIR_NAME: &str = ".agents";

/// `PI_PACKAGE_DIR` → `RPI_PACKAGE_DIR` (config.ts:369).
pub const ENV_PACKAGE_DIR: &str = "RPI_PACKAGE_DIR";

/// `getPackageDir` (config.ts:367-385): `RPI_PACKAGE_DIR` env override, else
/// the directory of the current executable (upstream Bun-binary rule; rpi is
/// always a native binary, so the Node `package.json` walk has no
/// counterpart).
pub fn get_package_dir() -> PathBuf {
    if let Some(env_dir) = std::env::var_os(ENV_PACKAGE_DIR) {
        if !env_dir.is_empty() {
            return PathBuf::from(normalize_path(&env_dir.to_string_lossy()));
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `getDocsPath` (config.ts): `{packageDir}/docs`.
pub fn get_docs_path() -> PathBuf {
    get_package_dir().join("docs")
}

/// Home directory of the current user (`HOME` / `USERPROFILE`).
pub fn user_home_dir() -> Option<PathBuf> {
    home_dir()
}

/// Global settings file: `{agentDir}/settings.json` (settings-manager.ts:195).
pub fn get_global_settings_path() -> PathBuf {
    get_agent_dir().join("settings.json")
}

/// Project config directory: `{cwd}/.rpi` (settings-manager.ts:196,
/// resource-loader.ts).
pub fn get_project_config_dir(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_DIR_NAME)
}

/// Project settings file: `{cwd}/.rpi/settings.json`.
pub fn get_project_settings_path(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("settings.json")
}

/// Global keybindings file: `{agentDir}/keybindings.json` (keybindings.ts;
/// global-only, there is no project-level keybindings file).
pub fn get_keybindings_path() -> PathBuf {
    get_agent_dir().join("keybindings.json")
}

/// Global skills directory: `{agentDir}/skills`.
pub fn get_global_skills_dir() -> PathBuf {
    get_agent_dir().join("skills")
}

/// Project skills directory: `{cwd}/.rpi/skills` (trust-gated).
pub fn get_project_skills_dir(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("skills")
}

/// Global Agent-Standard skills directory: `~/.agents/skills`
/// (excluded from the ancestor `.agents/skills` scan upstream).
pub fn get_global_agents_skills_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(AGENTS_DIR_NAME).join("skills"))
}

/// Global prompt templates directory: `{agentDir}/prompts`.
pub fn get_global_prompts_dir() -> PathBuf {
    get_agent_dir().join("prompts")
}

/// Project prompt templates directory: `{cwd}/.rpi/prompts` (trust-gated).
pub fn get_project_prompts_dir(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("prompts")
}

/// Global themes directory: `{agentDir}/themes`.
pub fn get_global_themes_dir() -> PathBuf {
    get_agent_dir().join("themes")
}

// ===== T14-W3: self-update (config.ts install-method section) =====
//
// Port of `detectInstallMethod` / `getSelfUpdateCommand` /
// `getSelfUpdateUnavailableInstruction` and helpers (config.ts:29-355)
// @ pi 0.82.1 (2efa728).
//
// Intentional differences (D-041):
// - rpi is always a native binary: upstream's `isBunBinary` detection
//   collapses to the path checks below, and the unmanaged case maps to the
//   upstream `bun-binary` outcome ("download from the releases page")
//   instead of `unknown`. There is no `__dirname`; the current executable
//   path takes the role of both `__dirname` and `process.argv[1]`.
// - `PACKAGE_NAME` is the npm/distribution package name rpi would be
//   installed under; there is no published package yet, so it is a single
//   constant here (the release endpoint may still redirect to another
//   package via `packageName`, like upstream).
// - Command probes (`pnpm root -g` etc.) go through an injectable reader
//   (`CommandProbe`) so tests never spawn.

/// `PACKAGE_NAME` (config.ts:488) — see the section note above.
pub const PACKAGE_NAME: &str = "rpi";

/// Download page printed when a standalone binary cannot self-update
/// (upstream `bun-binary` instruction, config.ts:336). Centralized here
/// for the W6 endpoint configuration pass.
pub const SELF_UPDATE_DOWNLOAD_URL: &str =
    "https://github.com/earendil-works/pi-mono/releases/latest";

/// `InstallMethod` (config.ts:29). `Binary` covers both upstream
/// `bun-binary` and `unknown` (indistinguishable for a native binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Binary,
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl InstallMethod {
    /// The name used in diagnostics (upstream `method` string interpolation).
    pub fn as_str(self) -> &'static str {
        match self {
            InstallMethod::Binary => "binary",
            InstallMethod::Npm => "npm",
            InstallMethod::Pnpm => "pnpm",
            InstallMethod::Yarn => "yarn",
            InstallMethod::Bun => "bun",
        }
    }
}

/// `SelfUpdateCommandStep` (config.ts:31-35).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateCommandStep {
    pub command: String,
    pub args: Vec<String>,
    pub display: String,
}

/// `SelfUpdateCommand` (config.ts:37-39): the install step plus optional
/// extra steps (the leading uninstall when the package was renamed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateCommand {
    pub step: SelfUpdateCommandStep,
    pub steps: Vec<SelfUpdateCommandStep>,
    pub display: String,
}

/// `SelfUpdatePackageTarget`, normalized (config.ts:41-51).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdatePackageTarget {
    pub package_name: String,
    pub install_spec: String,
}

impl SelfUpdatePackageTarget {
    /// String form: `packageName` doubles as the install spec.
    pub fn from_package_name(package_name: &str) -> Self {
        SelfUpdatePackageTarget {
            package_name: package_name.to_string(),
            install_spec: package_name.to_string(),
        }
    }
}

/// `readCommandOutput` injection seam (config.ts:189-204): run `command`
/// with `args`, returning trimmed stdout on exit code 0 (`None` when
/// empty), `Ok(None)` on failure unless `require_success`, then an
/// upstream-shaped error.
pub type CommandProbe<'a> = &'a dyn Fn(&str, &[&str], bool) -> Result<Option<String>, String>;

/// Production [`CommandProbe`] over `std::process`.
pub fn system_command_probe(
    command: &str,
    args: &[&str],
    require_success: bool,
) -> Result<Option<String>, String> {
    let display = std::iter::once(command)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let result = std::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match result {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(if stdout.is_empty() {
                None
            } else {
                Some(stdout)
            })
        }
        Ok(output) => {
            if !require_success {
                return Ok(None);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let reason = if stderr.is_empty() {
                format!(
                    "exit code {}",
                    output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            } else {
                stderr
            };
            Err(format!("Failed to run {display}: {reason}"))
        }
        Err(error) => {
            if require_success {
                Err(format!("Failed to run {display}: {error}"))
            } else {
                Ok(None)
            }
        }
    }
}

/// `detectInstallMethod` (config.ts:73-94) on the current executable path.
pub fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    detect_install_method_from(&exe)
}

/// The path-matching core of `detectInstallMethod` (test seam): upstream
/// joins `__dirname` and `process.execPath`; rpi has only the executable
/// path (see the section note).
pub fn detect_install_method_from(resolved_path: &str) -> InstallMethod {
    let resolved = resolved_path.to_lowercase().replace('\\', "/");
    if resolved.contains("/pnpm/") || resolved.contains("/.pnpm/") {
        return InstallMethod::Pnpm;
    }
    if resolved.contains("/yarn/") || resolved.contains("/.yarn/") {
        return InstallMethod::Yarn;
    }
    if resolved.contains("/install/global/node_modules/") {
        return InstallMethod::Bun;
    }
    if resolved.contains("/npm/") || resolved.contains("/node_modules/") {
        return InstallMethod::Npm;
    }
    InstallMethod::Binary
}

/// `makeSelfUpdateCommandStep` (config.ts:65-71): whitespace-bearing args
/// are quoted in the display string.
fn make_self_update_command_step(command: &str, args: &[String]) -> SelfUpdateCommandStep {
    let display = std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|arg| {
            if arg.chars().any(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    SelfUpdateCommandStep {
        command: command.to_string(),
        args: args.to_vec(),
        display,
    }
}

/// `makeSelfUpdateCommand` (config.ts:53-63).
fn make_self_update_command(
    install_step: SelfUpdateCommandStep,
    uninstall_step: Option<SelfUpdateCommandStep>,
) -> SelfUpdateCommand {
    match uninstall_step {
        None => SelfUpdateCommand {
            display: install_step.display.clone(),
            steps: vec![install_step.clone()],
            step: install_step,
        },
        Some(uninstall_step) => SelfUpdateCommand {
            display: format!("{} && {}", uninstall_step.display, install_step.display),
            steps: vec![uninstall_step, install_step.clone()],
            step: install_step,
        },
    }
}

/// The `match[1]` extraction of the pnpm branch (config.ts:128):
/// `/^(.*[\\/]global[\\/][^\\/]+)[\\/]\.pnpm[\\/]/`.
fn pnpm_global_root_from_package_dir(package_dir: &Path) -> Option<PathBuf> {
    let text = package_dir.to_string_lossy().replace('\\', "/");
    let index = text.find("/.pnpm/")?;
    let prefix = &text[..index];
    let (head, _last) = prefix.rsplit_once('/')?;
    if head.ends_with("/global") {
        Some(PathBuf::from(prefix))
    } else {
        None
    }
}

/// `getInferredNpmInstall` (config.ts:96-113): `(root, prefix)`.
fn get_inferred_npm_install() -> Option<(PathBuf, PathBuf)> {
    let package_dir = get_package_dir();
    let parent = package_dir.parent()?;
    let parent_name = parent.file_name()?.to_string_lossy();
    let root: PathBuf = if parent_name.starts_with('@')
        && parent
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "node_modules")
    {
        parent.parent()?.to_path_buf()
    } else if parent_name == "node_modules" {
        parent.to_path_buf()
    } else {
        return None;
    };
    let root_parent = root.parent()?.to_path_buf();
    if root_parent.file_name().is_some_and(|n| n == "lib") {
        Some((root, root_parent.parent()?.to_path_buf()))
    } else {
        // Windows custom prefixes are not inferred without `npm root -g`
        // evidence (config.ts:109-112).
        None
    }
}

/// `getSelfUpdateCommandForMethod` (config.ts:115-187).
pub fn get_self_update_command_for_method(
    method: InstallMethod,
    installed_package_name: &str,
    update_package_target: &SelfUpdatePackageTarget,
    npm_command: Option<&[String]>,
    probe: CommandProbe<'_>,
) -> Option<SelfUpdateCommand> {
    let target = update_package_target;
    let rename_uninstall = |command: &str, mut args: Vec<String>| {
        (target.package_name != installed_package_name).then(|| {
            args.push(installed_package_name.to_string());
            make_self_update_command_step(command, &args)
        })
    };
    match method {
        InstallMethod::Binary => None,
        InstallMethod::Pnpm => {
            let bin_dir_args: Vec<String> = match probe("pnpm", &["root", "-g"], false) {
                Ok(Some(_)) => Vec::new(),
                _ => match pnpm_global_root_from_package_dir(&get_package_dir()) {
                    Some(root) => {
                        let bin_dir = std::env::var("PNPM_HOME").ok().unwrap_or_else(|| {
                            root.parent()
                                .and_then(Path::parent)
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        });
                        vec![format!("--config.global-bin-dir={bin_dir}")]
                    }
                    None => Vec::new(),
                },
            };
            let mut install_args = vec![
                "install".to_string(),
                "-g".to_string(),
                "--ignore-scripts".to_string(),
                "--config.minimumReleaseAge=0".to_string(),
            ];
            install_args.extend(bin_dir_args.iter().cloned());
            install_args.push(target.install_spec.clone());
            let mut uninstall_args = vec!["remove".to_string(), "-g".to_string()];
            uninstall_args.extend(bin_dir_args);
            Some(make_self_update_command(
                make_self_update_command_step("pnpm", &install_args),
                rename_uninstall("pnpm", uninstall_args),
            ))
        }
        InstallMethod::Yarn => Some(make_self_update_command(
            make_self_update_command_step(
                "yarn",
                &[
                    "global".to_string(),
                    "add".to_string(),
                    "--ignore-scripts".to_string(),
                    target.install_spec.clone(),
                ],
            ),
            rename_uninstall("yarn", vec!["global".to_string(), "remove".to_string()]),
        )),
        InstallMethod::Bun => Some(make_self_update_command(
            make_self_update_command_step(
                "bun",
                &[
                    "install".to_string(),
                    "-g".to_string(),
                    "--ignore-scripts".to_string(),
                    "--minimum-release-age=0".to_string(),
                    target.install_spec.clone(),
                ],
            ),
            rename_uninstall("bun", vec!["uninstall".to_string(), "-g".to_string()]),
        )),
        InstallMethod::Npm => {
            let (command, npm_args): (String, Vec<String>) = match npm_command {
                Some(command) if !command.is_empty() => (command[0].clone(), command[1..].to_vec()),
                _ => ("npm".to_string(), Vec::new()),
            };
            let configured = npm_command.is_some_and(|c| !c.is_empty());
            let inferred = if configured {
                None
            } else {
                get_inferred_npm_install()
            };
            let mut prefix_args = npm_args;
            if let Some((_, prefix)) = inferred {
                prefix_args.push("--prefix".to_string());
                prefix_args.push(prefix.to_string_lossy().into_owned());
            }
            let mut install_args = prefix_args.clone();
            install_args.extend([
                "install".to_string(),
                "-g".to_string(),
                "--ignore-scripts".to_string(),
                "--min-release-age=0".to_string(),
                target.install_spec.clone(),
            ]);
            let mut uninstall_args = prefix_args;
            uninstall_args.extend(["uninstall".to_string(), "-g".to_string()]);
            Some(make_self_update_command(
                make_self_update_command_step(&command, &install_args),
                rename_uninstall(&command, uninstall_args),
            ))
        }
    }
}

/// `getGlobalPackageRoots` (config.ts:206-249).
fn get_global_package_roots(
    method: InstallMethod,
    npm_command: Option<&[String]>,
    probe: CommandProbe<'_>,
) -> Vec<PathBuf> {
    let probed = |command: &str, args: &[&str], require_success: bool| {
        probe(command, args, require_success).ok().flatten()
    };
    match method {
        InstallMethod::Npm => {
            let configured = npm_command.is_some_and(|c| !c.is_empty());
            let (command, npm_args): (&str, Vec<String>) = match npm_command {
                Some(command) if !command.is_empty() => {
                    (command[0].as_str(), command[1..].to_vec())
                }
                _ => ("npm", Vec::new()),
            };
            let arg_refs: Vec<&str> = npm_args.iter().map(String::as_str).collect();
            if configured && command == "bun" {
                let mut bun_args = arg_refs.clone();
                bun_args.extend(["pm", "bin", "-g"]);
                let bun_bin = probed(command, &bun_args, true);
                let mut roots = Vec::new();
                if let Some(home) = user_home_dir() {
                    roots.push(
                        home.join(".bun")
                            .join("install")
                            .join("global")
                            .join("node_modules"),
                    );
                }
                if let Some(bun_bin) = bun_bin {
                    if let Some(parent) = Path::new(&bun_bin).parent() {
                        roots.push(parent.join("install").join("global").join("node_modules"));
                    }
                }
                return roots;
            }
            let mut root_args = arg_refs;
            root_args.extend(["root", "-g"]);
            let root = probed(command, &root_args, configured);
            let inferred = if configured {
                None
            } else {
                get_inferred_npm_install()
            };
            let mut roots = Vec::new();
            if let Some(root) = root {
                roots.push(PathBuf::from(root));
            }
            if let Some((inferred_root, _)) = inferred {
                roots.push(inferred_root);
            }
            roots
        }
        InstallMethod::Pnpm => {
            if let Some(root) = probed("pnpm", &["root", "-g"], false) {
                let root = PathBuf::from(root);
                let mut roots = vec![root.clone()];
                if let Some(parent) = root.parent() {
                    roots.push(parent.to_path_buf());
                }
                return roots;
            }
            pnpm_global_root_from_package_dir(&get_package_dir())
                .into_iter()
                .collect()
        }
        InstallMethod::Yarn => match probed("yarn", &["global", "dir"], false) {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                vec![dir.clone(), dir.join("node_modules")]
            }
            None => Vec::new(),
        },
        InstallMethod::Bun => {
            let bun_bin = probed("bun", &["pm", "bin", "-g"], false);
            let mut roots = Vec::new();
            if let Some(home) = user_home_dir() {
                roots.push(
                    home.join(".bun")
                        .join("install")
                        .join("global")
                        .join("node_modules"),
                );
            }
            if let Some(bun_bin) = bun_bin {
                if let Some(parent) = Path::new(&bun_bin).parent() {
                    roots.push(parent.join("install").join("global").join("node_modules"));
                }
            }
            roots
        }
        InstallMethod::Binary => Vec::new(),
    }
}

/// `normalizeExistingPathForComparison` (config.ts:251-268).
fn normalize_existing_path_for_comparison(path: &Path, resolve_symlinks: bool) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolved = resolve_path(&path.to_string_lossy(), &cwd);
    if !resolved.exists() {
        return None;
    }
    let mut normalized = if resolve_symlinks {
        std::fs::canonicalize(&resolved).ok()?
    } else {
        resolved
    };
    if cfg!(windows) {
        normalized = PathBuf::from(normalized.to_string_lossy().to_lowercase());
    }
    Some(normalized.to_string_lossy().into_owned())
}

/// `getPathComparisonCandidates` (config.ts:270-278).
fn get_path_comparison_candidates(path: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    for resolve_symlinks in [false, true] {
        if let Some(candidate) = normalize_existing_path_for_comparison(path, resolve_symlinks) {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// `getEntrypointPackageDir` (config.ts:280-291): walk up from the
/// executable looking for `package.json` (upstream walks from
/// `process.argv[1]`; see the section note).
fn get_entrypoint_package_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    loop {
        if dir.join("package.json").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `isSelfUpdatePathWritable` (config.ts:293-302).
fn is_self_update_path_writable() -> bool {
    let package_dir = get_package_dir();
    let writable = |path: &Path| {
        #[cfg(unix)]
        {
            let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes());
            match c_path {
                Ok(c_path) => (unsafe { libc::access(c_path.as_ptr(), libc::W_OK) }) == 0,
                Err(_) => false,
            }
        }
        #[cfg(not(unix))]
        {
            // Best-effort fallback: the directory must exist and not be
            // flagged read-only (Windows is not a v0.1 target).
            path.metadata()
                .map(|m| m.is_dir() && !m.permissions().readonly())
                .unwrap_or(false)
        }
    };
    let parent = package_dir.parent().map(Path::to_path_buf);
    writable(&package_dir) && parent.as_deref().is_some_and(writable)
}

/// `isManagedByGlobalPackageManager` (config.ts:304-313).
fn is_managed_by_global_package_manager(
    method: InstallMethod,
    npm_command: Option<&[String]>,
    probe: CommandProbe<'_>,
) -> bool {
    let mut package_dirs = vec![get_package_dir()];
    if let Some(entrypoint_dir) = get_entrypoint_package_dir() {
        package_dirs.push(entrypoint_dir);
    }
    let package_dir_candidates: Vec<String> = package_dirs
        .iter()
        .flat_map(|dir| get_path_comparison_candidates(dir))
        .collect();
    get_global_package_roots(method, npm_command, probe)
        .iter()
        .flat_map(|root| get_path_comparison_candidates(root))
        .any(|normalized_root| {
            let root_prefix = if normalized_root.ends_with(std::path::MAIN_SEPARATOR) {
                normalized_root.clone()
            } else {
                format!("{normalized_root}{}", std::path::MAIN_SEPARATOR)
            };
            package_dir_candidates
                .iter()
                .any(|package_dir| package_dir.starts_with(&root_prefix))
        })
}

/// `getSelfUpdateCommand` (config.ts:315-326).
pub fn get_self_update_command(
    package_name: &str,
    npm_command: Option<&[String]>,
    update_package_target: &SelfUpdatePackageTarget,
    probe: CommandProbe<'_>,
) -> Option<SelfUpdateCommand> {
    let method = detect_install_method();
    let command = get_self_update_command_for_method(
        method,
        package_name,
        update_package_target,
        npm_command,
        probe,
    )?;
    if !is_managed_by_global_package_manager(method, npm_command, probe)
        || !is_self_update_path_writable()
    {
        return None;
    }
    Some(command)
}

/// `getSelfUpdateUnavailableInstruction` (config.ts:328-346).
pub fn get_self_update_unavailable_instruction(
    package_name: &str,
    npm_command: Option<&[String]>,
    update_package_target: &SelfUpdatePackageTarget,
    probe: CommandProbe<'_>,
) -> String {
    let method = detect_install_method();
    if method == InstallMethod::Binary {
        return format!("Download from: {SELF_UPDATE_DOWNLOAD_URL}");
    }
    let command = get_self_update_command_for_method(
        method,
        package_name,
        update_package_target,
        npm_command,
        probe,
    );
    match command {
        Some(command) => {
            if is_managed_by_global_package_manager(method, npm_command, probe)
                && !is_self_update_path_writable()
            {
                format!(
                    "This installation is managed by a global {} install, but the install path is not writable. Update it yourself with: {}",
                    method.as_str(),
                    command.display
                )
            } else {
                format!(
                    "This installation is not managed by a global {} install. Update it with the package manager, wrapper, or source checkout that provides it.",
                    method.as_str()
                )
            }
        }
        None => format!(
            "Update {} using the package manager, wrapper, or source checkout that provides this installation.",
            update_package_target.install_spec
        ),
    }
}

/// Project themes directory: `{cwd}/.rpi/themes` (trust-gated).
pub fn get_project_themes_dir(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("themes")
}

// ===== T14-W6a: configurable product endpoints (ADR-0002 §8) =====
//
// The three product HTTP callbacks — version check, install telemetry, and
// the remote model catalog — resolve their endpoint from the environment
// first, then settings, then the built-in default (`https://resetpi.com`). Any
// level set to the literal `off` (ASCII case-insensitive, trimmed) disables
// the endpoint entirely: a disabled endpoint must produce **no** network
// request. These env vars and the `versionCheckUrl` / `telemetryUrl` /
// `modelCatalogUrl` settings keys are rpi-specific (ADR-0002 §8); upstream
// hardcodes the URLs. The env-over-settings precedence mirrors the one
// upstream override of this kind (`PI_TELEMETRY` over
// `enableInstallTelemetry`, telemetry.ts:10-12).

/// Rpi-specific (ADR-0002 §8): override for the version-check endpoint
/// (default `LATEST_VERSION_URL` in [`crate::core::version_check`]).
pub const ENV_VERSION_CHECK_URL: &str = "RPI_VERSION_CHECK_URL";
/// Rpi-specific (ADR-0002 §8): override for the install-telemetry endpoint
/// (default `DEFAULT_REPORT_INSTALL_URL` in [`crate::core::telemetry`]).
pub const ENV_TELEMETRY_URL: &str = "RPI_TELEMETRY_URL";
/// Rpi-specific (ADR-0002 §8): override for the remote model catalog base
/// URL (default `DEFAULT_CATALOG_BASE_URL` in
/// [`crate::core::remote_catalog_provider`]).
pub const ENV_MODEL_CATALOG_URL: &str = "RPI_MODEL_CATALOG_URL";

/// Endpoint resolution shared by all three product endpoints: a non-empty
/// env value wins over a non-empty settings value, which wins over the
/// default (empty strings fall through, JS `||` semantics); the literal
/// `off` disables the endpoint (`None` — callers must not perform any
/// network request).
pub fn resolve_endpoint(
    env_value: Option<&str>,
    settings_value: Option<&str>,
    default: &str,
) -> Option<String> {
    let candidate = env_value
        .filter(|value| !value.is_empty())
        .or_else(|| settings_value.filter(|value| !value.is_empty()));
    match candidate {
        Some(value) if value.trim().eq_ignore_ascii_case("off") => None,
        Some(value) => Some(value.to_string()),
        None => Some(default.to_string()),
    }
}

/// [`resolve_endpoint`] reading `env_name` from the process environment.
pub fn endpoint_from_env(
    env_name: &str,
    settings_value: Option<&str>,
    default: &str,
) -> Option<String> {
    resolve_endpoint(
        std::env::var(env_name).ok().as_deref(),
        settings_value,
        default,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_cwd_dir_name_unix_absolute() {
        assert_eq!(
            encode_cwd_dir_name("/home/leven/repo"),
            "--home-leven-repo--"
        );
        assert_eq!(encode_cwd_dir_name("/"), "----");
    }

    #[test]
    fn test_encode_cwd_dir_name_windows_drive_colon() {
        // Windows drive-letter colon and backslashes are replaced (self-check
        // list: 目录编码规则含 Windows 盘符冒号).
        assert_eq!(
            encode_cwd_dir_name("C:\\Users\\dev\\repo"),
            "--C--Users-dev-repo--"
        );
        assert_eq!(encode_cwd_dir_name("C:/Users/dev"), "--C--Users-dev--");
    }

    #[test]
    fn test_encode_cwd_dir_name_relative_after_resolve() {
        // get_default_session_dir_path resolves first; a trailing colon
        // anywhere is replaced, not just the drive letter.
        assert_eq!(encode_cwd_dir_name("/a:b/c"), "--a-b-c--");
    }

    #[test]
    fn test_resolve_session_dir_chain_priority() {
        // flag > env > settings > default(None) (main.ts:573-577).
        let flag = Some("/flag");
        let env = Some("/env");
        let settings = Some("/settings");
        assert_eq!(
            resolve_session_dir(flag, env, settings),
            Some(PathBuf::from("/flag"))
        );
        assert_eq!(
            resolve_session_dir(None, env, settings),
            Some(PathBuf::from("/env"))
        );
        assert_eq!(
            resolve_session_dir(None, None, settings),
            Some(PathBuf::from("/settings"))
        );
        assert_eq!(resolve_session_dir(None, None, None), None);
    }

    #[test]
    fn test_resolve_session_dir_empty_string_falls_through() {
        // Empty strings are falsy upstream (`parsed.sessionDir ? … :
        // undefined`, main.ts:573-576) and must not win their level.
        assert_eq!(
            resolve_session_dir(Some(""), Some("/env"), Some("/settings")),
            Some(PathBuf::from("/env"))
        );
        assert_eq!(
            resolve_session_dir(Some(""), Some(""), Some("/settings")),
            Some(PathBuf::from("/settings"))
        );
        assert_eq!(resolve_session_dir(Some(""), Some(""), None), None);
    }

    #[test]
    fn test_resolve_session_dir_expands_tilde() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(
                resolve_session_dir(Some("~/sessions"), None, None),
                Some(home.join("sessions"))
            );
        }
    }

    #[test]
    fn test_get_default_session_dir_path_encoding() {
        let agent_dir = Path::new("/tmp/agent");
        let dir = get_default_session_dir_path(Path::new("/some/project"), Some(agent_dir));
        assert_eq!(dir, PathBuf::from("/tmp/agent/sessions/--some-project--"));
    }

    // ---- T14-W6a: product endpoint resolution (ADR-0002 §8) ----

    #[test]
    fn test_resolve_endpoint_defaults_without_overrides() {
        assert_eq!(
            resolve_endpoint(None, None, "https://resetpi.com/api"),
            Some("https://resetpi.com/api".to_string())
        );
        // Empty strings fall through to the next level (JS `||` semantics).
        assert_eq!(
            resolve_endpoint(Some(""), Some(""), "https://resetpi.com/api"),
            Some("https://resetpi.com/api".to_string())
        );
    }

    #[test]
    fn test_resolve_endpoint_env_beats_settings_beats_default() {
        assert_eq!(
            resolve_endpoint(
                Some("https://env.test"),
                Some("https://settings.test"),
                "https://default.test"
            ),
            Some("https://env.test".to_string())
        );
        assert_eq!(
            resolve_endpoint(None, Some("https://settings.test"), "https://default.test"),
            Some("https://settings.test".to_string())
        );
        // An empty env value falls through to the settings level.
        assert_eq!(
            resolve_endpoint(
                Some(""),
                Some("https://settings.test"),
                "https://default.test"
            ),
            Some("https://settings.test".to_string())
        );
    }

    #[test]
    fn test_resolve_endpoint_off_disables() {
        // `off` (trimmed, ASCII case-insensitive) at either level disables.
        assert_eq!(
            resolve_endpoint(Some("off"), None, "https://default.test"),
            None
        );
        assert_eq!(
            resolve_endpoint(None, Some(" OFF "), "https://default.test"),
            None
        );
        // An env `off` wins over a settings URL (env precedence).
        assert_eq!(
            resolve_endpoint(
                Some("off"),
                Some("https://settings.test"),
                "https://default.test"
            ),
            None
        );
        // A settings `off` wins over the default.
        assert_eq!(
            resolve_endpoint(None, Some("off"), "https://default.test"),
            None
        );
        // But an env URL beats a settings `off` (env precedence both ways).
        assert_eq!(
            resolve_endpoint(
                Some("https://env.test"),
                Some("off"),
                "https://default.test"
            ),
            Some("https://env.test".to_string())
        );
    }
}

#[cfg(test)]
mod self_update_tests {
    //! Port of the install-method / self-update-command intent of
    //! `packages/coding-agent/test` self-update coverage (config.ts:29-355).

    use super::*;

    fn target(package_name: &str, install_spec: &str) -> SelfUpdatePackageTarget {
        SelfUpdatePackageTarget {
            package_name: package_name.to_string(),
            install_spec: install_spec.to_string(),
        }
    }

    fn no_probe(_: &str, _: &[&str], _: bool) -> Result<Option<String>, String> {
        Ok(None)
    }

    #[test]
    fn detect_install_method_from_paths() {
        assert_eq!(
            detect_install_method_from(
                "/home/u/.local/share/pnpm/global/5/.pnpm/rpi@1.0.0/node_modules/rpi/bin"
            ),
            InstallMethod::Pnpm
        );
        assert_eq!(
            detect_install_method_from("/home/u/.yarn/global/node_modules/.bin/rpi"),
            InstallMethod::Yarn
        );
        assert_eq!(
            detect_install_method_from("/home/u/.bun/install/global/node_modules/rpi/bin/rpi"),
            InstallMethod::Bun
        );
        assert_eq!(
            detect_install_method_from("/usr/lib/node_modules/rpi/bin/rpi"),
            InstallMethod::Npm
        );
        assert_eq!(
            detect_install_method_from("/usr/local/bin/rpi"),
            InstallMethod::Binary
        );
        // Case-insensitive, Windows separators tolerated.
        assert_eq!(
            detect_install_method_from("C:\\Users\\u\\AppData\\Roaming\\NPM\\rpi.exe"),
            InstallMethod::Npm
        );
    }

    #[test]
    fn self_update_command_display_quotes_whitespace_args() {
        let step = make_self_update_command_step(
            "npm",
            &[
                "install".to_string(),
                "-g".to_string(),
                "my pkg@1.0.0".to_string(),
            ],
        );
        assert_eq!(step.display, r#"npm install -g "my pkg@1.0.0""#);
    }

    #[test]
    fn npm_self_update_command_shape() {
        let command = get_self_update_command_for_method(
            InstallMethod::Npm,
            "rpi",
            &target("rpi", "rpi@1.2.3"),
            None,
            &no_probe,
        )
        .expect("command");
        assert_eq!(command.step.command, "npm");
        assert_eq!(
            command.step.args,
            [
                "install",
                "-g",
                "--ignore-scripts",
                "--min-release-age=0",
                "rpi@1.2.3"
            ]
        );
        // Same package: no uninstall step.
        assert_eq!(command.steps.len(), 1);
        assert_eq!(command.display, command.step.display);
    }

    #[test]
    fn renamed_package_prepends_uninstall_step() {
        let command = get_self_update_command_for_method(
            InstallMethod::Npm,
            "rpi",
            &target("rpi-next", "rpi-next@1.2.3"),
            None,
            &no_probe,
        )
        .expect("command");
        assert_eq!(command.steps.len(), 2);
        assert_eq!(command.steps[0].args, ["uninstall", "-g", "rpi"]);
        assert!(command.display.contains(" && "));
    }

    #[test]
    fn pnpm_yarn_bun_command_shapes() {
        let pnpm = get_self_update_command_for_method(
            InstallMethod::Pnpm,
            "rpi",
            &target("rpi", "rpi@1.2.3"),
            None,
            &no_probe,
        )
        .expect("pnpm");
        assert_eq!(pnpm.step.command, "pnpm");
        assert!(pnpm
            .step
            .args
            .contains(&"--config.minimumReleaseAge=0".to_string()));

        let yarn = get_self_update_command_for_method(
            InstallMethod::Yarn,
            "rpi",
            &target("rpi", "rpi@1.2.3"),
            None,
            &no_probe,
        )
        .expect("yarn");
        assert_eq!(
            yarn.step.args,
            ["global", "add", "--ignore-scripts", "rpi@1.2.3"]
        );

        let bun = get_self_update_command_for_method(
            InstallMethod::Bun,
            "rpi",
            &target("rpi", "rpi@1.2.3"),
            None,
            &no_probe,
        )
        .expect("bun");
        assert!(bun
            .step
            .args
            .contains(&"--minimum-release-age=0".to_string()));
    }

    #[test]
    fn npm_command_wrapper_replaces_the_command() {
        let npm_command = vec!["bun".to_string(), "--bun".to_string()];
        let command = get_self_update_command_for_method(
            InstallMethod::Npm,
            "rpi",
            &target("rpi", "rpi@1.2.3"),
            Some(&npm_command),
            &no_probe,
        )
        .expect("command");
        assert_eq!(command.step.command, "bun");
        assert_eq!(command.step.args[0], "--bun");
    }

    #[test]
    fn binary_install_has_no_command_and_download_instruction() {
        assert!(get_self_update_command_for_method(
            InstallMethod::Binary,
            "rpi",
            &target("rpi", "rpi@1.2.3"),
            None,
            &no_probe,
        )
        .is_none());
        // The current executable is a standalone binary (test runner), so
        // the instruction is the download URL (upstream bun-binary branch).
        let instruction = get_self_update_unavailable_instruction(
            "rpi",
            None,
            &target("rpi", "rpi@1.2.3"),
            &no_probe,
        );
        assert_eq!(
            instruction,
            format!("Download from: {SELF_UPDATE_DOWNLOAD_URL}")
        );
    }

    #[test]
    fn share_viewer_url_default_override_and_empty_fallback() {
        // Single test for all env manipulation — parallel tests would race
        // on `RPI_SHARE_VIEWER_URL`.
        std::env::remove_var(ENV_SHARE_VIEWER_URL);
        assert_eq!(
            get_share_viewer_url("abc123"),
            "https://resetpi.com/session/#abc123"
        );
        std::env::set_var(ENV_SHARE_VIEWER_URL, "https://viewer.example.com/s/");
        assert_eq!(
            get_share_viewer_url("abc123"),
            "https://viewer.example.com/s/#abc123"
        );
        // Empty env value is falsy upstream (`||` fallback, config.ts:506).
        std::env::set_var(ENV_SHARE_VIEWER_URL, "");
        assert_eq!(
            get_share_viewer_url("abc123"),
            "https://resetpi.com/session/#abc123"
        );
        std::env::remove_var(ENV_SHARE_VIEWER_URL);
    }

    #[test]
    fn pnpm_global_root_extraction() {
        let root = pnpm_global_root_from_package_dir(Path::new(
            "/home/u/.local/share/pnpm/global/5/.pnpm/rpi@1.0.0/node_modules/rpi",
        ));
        assert_eq!(
            root,
            Some(PathBuf::from("/home/u/.local/share/pnpm/global/5"))
        );
        assert!(pnpm_global_root_from_package_dir(Path::new("/usr/local/bin")).is_none());
    }
}
