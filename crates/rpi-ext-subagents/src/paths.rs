//! Path resolution for the subagents extension (single module, coding-standards §10.1).
//!
//! Port of the path helpers this plugin needs from pi-subagents
//! `src/shared/utils.ts` + `src/shared/types.ts` @ v0.48.0 (56f97234), with the
//! ADR-0001 mapping applied: `~/.pi` → `~/.rpi`, `.pi` → `.rpi`,
//! `PI_*` → `RPI_*`. The plugin never reads `~/.pi` / `.pi`.
//!
//! Intentional differences: rpi's `RPI_CODING_AGENT_DIR` env replaces upstream
//! `PI_CODING_AGENT_DIR`; the project config dir name is the fixed `.rpi`
//! (upstream allows the host `piConfig.configDir` to override it — rpi has no
//! such override).

use std::path::{Path, PathBuf};

pub const ENV_AGENT_DIR: &str = "RPI_CODING_AGENT_DIR";
pub const ENV_SESSION_DIR: &str = "RPI_CODING_AGENT_SESSION_DIR";
pub const CONFIG_DIR_NAME: &str = ".rpi";
pub const PROJECT_SUBAGENTS_RELATIVE_DIR: &str = ".rpi/subagents";

/// `getAgentDir` equivalent (utils.ts:97-102, `PI_CODING_AGENT_DIR ?? ~/.pi/agent`
/// → `RPI_CODING_AGENT_DIR ?? ~/.rpi/agent`).
pub fn get_agent_dir() -> PathBuf {
    if let Some(env_dir) = std::env::var_os(ENV_AGENT_DIR) {
        if !env_dir.is_empty() {
            return normalize_path(&env_dir.to_string_lossy());
        }
    }
    match home_dir() {
        Some(home) => home.join(CONFIG_DIR_NAME).join("agent"),
        None => PathBuf::from(CONFIG_DIR_NAME).join("agent"),
    }
}

/// `getProjectConfigDir` (utils.ts:120-124): `<root>/.pi` → `<root>/.rpi`.
pub fn get_project_config_dir(project_root: &Path) -> PathBuf {
    project_root.join(CONFIG_DIR_NAME)
}

/// Home directory from HOME/USERPROFILE (upstream `os.homedir()` on unix uses
/// HOME; the plugin keeps the two-variable form TE-D03 established for
/// mcp-adapter, no passwd fallback).
pub fn home_dir() -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// `normalizePath` (utils/paths.ts): `~/…` expansion plus lexical cleanup.
/// The upstream implementation expands `~`, collapses `.`/`..` and duplicate
/// separators without touching symlinks; `PathBuf` components give the same
/// lexical result for the inputs this plugin produces.
pub fn normalize_path(input: &str) -> PathBuf {
    let expanded = if let Some(rest) = input.strip_prefix("~/") {
        match home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(rest),
        }
    } else if input == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else {
        PathBuf::from(input)
    };
    let mut out = PathBuf::new();
    for component in expanded.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `expandTilde` equivalent used for `sessionDir` / `defaultSessionDir`
/// (executor 5930-5936): resolve `~` then make absolute against the process cwd.
pub fn expand_tilde_and_resolve(input: &str) -> PathBuf {
    let normalized = normalize_path(input);
    if normalized.is_absolute() {
        normalized
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(normalized)
    }
}

/// Temp scope id (`resolveTempScopeId`, types.ts:1977-2020). `uid-<uid>` on
/// unix (process.getuid is always available there), then USERNAME/USER/LOGNAME,
/// then HOME/USERPROFILE, then `shared`.
pub fn resolve_temp_scope_id() -> String {
    #[cfg(unix)]
    {
        // Safety: getuid never fails.
        let uid = unsafe { libc::getuid() };
        format!("uid-{uid}")
    }
    #[cfg(not(unix))]
    {
        for key in ["USERNAME", "USER", "LOGNAME"] {
            if let Some(value) = std::env::var(key).ok().filter(|v| !v.is_empty()) {
                return format!("user-{}", sanitize_temp_scope_segment(&value));
            }
        }
        if let Some(home) = home_dir() {
            return format!(
                "home-{}",
                sanitize_temp_scope_segment(&home.to_string_lossy())
            );
        }
        "shared".to_string()
    }
}

#[cfg_attr(unix, allow(dead_code))]
fn sanitize_temp_scope_segment(value: &str) -> String {
    // types.ts:1969-1975: collapse non [A-Za-z0-9._-] runs to `-`, trim edge
    // dashes, fall back to `unknown`.
    let mut out = String::new();
    let mut in_run = false;
    for c in value.trim().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn temp_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `TEMP_ROOT_DIR` (types.ts:2024): `<tmp>/pi-subagents-<scope>` →
/// `<tmp>/rpi-subagents-<scope>`.
pub fn temp_root_dir() -> PathBuf {
    temp_dir().join(format!("rpi-subagents-{}", resolve_temp_scope_id()))
}

/// `TEMP_ARTIFACTS_DIR` (types.ts:2028).
pub fn temp_artifacts_dir() -> PathBuf {
    temp_root_dir().join("artifacts")
}

/// `getProjectSubagentsDir` (artifacts.ts:133-135) with the ADR-0001 root swap.
pub fn get_project_subagents_dir(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_SUBAGENTS_RELATIVE_DIR)
}

/// `getProjectArtifactsDir` (artifacts.ts:137-139).
pub fn get_project_artifacts_dir(cwd: &Path) -> PathBuf {
    get_project_subagents_dir(cwd).join("artifacts")
}

/// Encode a resolved cwd into the session subdirectory name, mirroring the
/// host rule (rpi `config.rs:150-162`, session-manager.ts:476-481): strip one
/// leading `/`, replace `/`, `\`, `:` with `-`, wrap in `--…--`.
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

/// Resolve the parent session storage directory the host would use for `cwd`:
/// `RPI_CODING_AGENT_SESSION_DIR` env > `settings.sessionDir` (plugin reads the
/// same two settings files, project wins) > `<agentDir>/sessions/--<cwd>--`.
///
/// The plugin cannot ask the host for its session file (no ABI method; TE04
/// must not change rpi-ext-host), so fresh/fork session roots and the parent
/// session lookup are derived from this deterministic layout instead of
/// `ctx.sessionManager` upstream uses. See TE04 deviation TE-D16.
pub fn resolve_parent_session_dir(cwd: &Path, settings_session_dir: Option<&str>) -> PathBuf {
    if let Some(env_dir) = std::env::var_os(ENV_SESSION_DIR) {
        if !env_dir.is_empty() {
            return normalize_path(&env_dir.to_string_lossy());
        }
    }
    if let Some(configured) = settings_session_dir.filter(|s| !s.is_empty()) {
        return expand_tilde_and_resolve(configured);
    }
    get_agent_dir()
        .join("sessions")
        .join(encode_cwd_dir_name(&cwd.to_string_lossy()))
}

/// `findLatestSessionFile` (utils.ts:268-282): newest `*.jsonl` in `dir` by
/// mtime; `None` when the directory has none.
pub fn find_latest_session_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(best_time, _)| modified > *best_time)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_scope_segment_sanitizes_like_upstream() {
        assert_eq!(sanitize_temp_scope_segment("leven"), "leven");
        assert_eq!(sanitize_temp_scope_segment("a b!!c"), "a-b-c");
        assert_eq!(sanitize_temp_scope_segment("---"), "unknown");
        assert_eq!(sanitize_temp_scope_segment(""), "unknown");
    }

    #[test]
    fn encode_cwd_dir_name_matches_host_rule() {
        assert_eq!(encode_cwd_dir_name("/home/x/repo"), "--home-x-repo--");
        assert_eq!(encode_cwd_dir_name("C:\\dev\\repo"), "--C--dev-repo--");
    }

    #[test]
    fn normalize_path_expands_tilde_and_dots() {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let p = normalize_path("~/a/./b/../c");
        assert_eq!(p, home.join("a/c"));
    }
}
