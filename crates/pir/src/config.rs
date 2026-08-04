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
//! - `APP_NAME` is `pir`, `CONFIG_DIR_NAME` is `.pir`, the env prefix is
//!   `PIR_` (upstream: `pi` / `.pi` / `PI_`).
//! - Home directory comes from `HOME` (unix) / `USERPROFILE` (windows)
//!   directly; there is no `os.homedir()` equivalent in the dependency
//!   baseline.

use std::path::{Path, PathBuf};

use crate::tools::path_utils::{normalize_path, resolve_path};

/// `APP_NAME` (config.ts:489) — Pir rename (ADR-0001).
pub const APP_NAME: &str = "pir";
/// `CONFIG_DIR_NAME` (config.ts:491) — Pir rename (ADR-0001).
pub const CONFIG_DIR_NAME: &str = ".pir";
/// `ENV_AGENT_DIR` = `{APP_NAME}_CODING_AGENT_DIR` (config.ts:495).
pub const ENV_AGENT_DIR: &str = "PIR_CODING_AGENT_DIR";
/// `ENV_SESSION_DIR` = `{APP_NAME}_CODING_AGENT_SESSION_DIR` (config.ts:496).
pub const ENV_SESSION_DIR: &str = "PIR_CODING_AGENT_SESSION_DIR";
/// `PI_OFFLINE` → `PIR_OFFLINE` (requirements §3.3).
pub const ENV_OFFLINE: &str = "PIR_OFFLINE";
/// `PI_SKIP_VERSION_CHECK` → `PIR_SKIP_VERSION_CHECK` (requirements §3.3).
pub const ENV_SKIP_VERSION_CHECK: &str = "PIR_SKIP_VERSION_CHECK";
/// Package version (`VERSION` in config.ts — from package.json upstream).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
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

/// `getAgentDir` (config.ts:515-521): `PIR_CODING_AGENT_DIR` env override,
/// else `~/.pir/agent`.
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
/// (main.ts:573-577): `--session-dir` flag > `PIR_CODING_AGENT_SESSION_DIR`
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
/// `PIR_CODING_AGENT_SESSION_DIR` from the process environment.
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

/// `PI_PACKAGE_DIR` → `PIR_PACKAGE_DIR` (config.ts:369).
pub const ENV_PACKAGE_DIR: &str = "PIR_PACKAGE_DIR";

/// `getPackageDir` (config.ts:367-385): `PIR_PACKAGE_DIR` env override, else
/// the directory of the current executable (upstream Bun-binary rule; pir is
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

/// Project config directory: `{cwd}/.pir` (settings-manager.ts:196,
/// resource-loader.ts).
pub fn get_project_config_dir(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_DIR_NAME)
}

/// Project settings file: `{cwd}/.pir/settings.json`.
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

/// Project skills directory: `{cwd}/.pir/skills` (trust-gated).
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

/// Project prompt templates directory: `{cwd}/.pir/prompts` (trust-gated).
pub fn get_project_prompts_dir(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("prompts")
}

/// Global themes directory: `{agentDir}/themes`.
pub fn get_global_themes_dir() -> PathBuf {
    get_agent_dir().join("themes")
}

/// Project themes directory: `{cwd}/.pir/themes` (trust-gated).
pub fn get_project_themes_dir(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("themes")
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
}
