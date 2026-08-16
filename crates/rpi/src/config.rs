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
pub const DEFAULT_SHARE_VIEWER_URL: &str = "https://revpi.dev/session/";

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
// The port of `detectInstallMethod` / `getSelfUpdateCommand` /
// `getSelfUpdateUnavailableInstruction` and helpers (config.ts:29-355)
// @ pi 0.82.1 (2efa728) was removed per the ADR-0011 revision
// (2026-08-10, deviation D-055): rpi's only distribution channel is the
// GitHub Releases binary, so the npm/pnpm/yarn/bun install-method branches
// were unreachable dead code and the only self-update is the Binary
// download path (T18). The unmanaged case maps to the upstream
// `bun-binary` outcome ("download from the releases page").
//
// The version-probe contract is unchanged (D-041): `PACKAGE_NAME` is the
// distribution package name, and the release endpoint may still redirect
// to another package via `packageName`, like upstream.

/// `PACKAGE_NAME` (config.ts:488) — see the section note above.
pub const PACKAGE_NAME: &str = "rpi";

/// Last-resort download page for manual installs/updates (T18, ADR-0011
/// §6): binary installs self-update via `rpi update --self` and only land
/// on this URL when the build target triple is unknown. Centralized here
/// for the W6 endpoint configuration pass.
pub const SELF_UPDATE_DOWNLOAD_URL: &str = "https://github.com/revpidev/rpi/releases/latest";

/// Project themes directory: `{cwd}/.rpi/themes` (trust-gated).
pub fn get_project_themes_dir(cwd: &Path) -> PathBuf {
    get_project_config_dir(cwd).join("themes")
}

// ===== T18: binary distribution lifecycle (ADR-0011 §3/§4/§6) =====
//
// No upstream counterpart: rpi's only real install shape is the GitHub
// Releases binary (ADR-0011 §1), so the install manifest and the
// build-target injection below are rpi-specific. The manifest lives next
// to the executable (`~/.local/bin/rpi.install.json`), located via
// `current_exe()`; it is deliberately NOT coupled to
// `RPI_CODING_AGENT_DIR` (the agent data dir has separate semantics).

/// Build-time target triple injected by build.rs (`cargo:rustc-env`,
/// ADR-0011 §4). `None` only for non-cargo builds; consumers must then
/// fall back to manual-download guidance and never guess glibc vs musl.
pub fn build_target() -> Option<&'static str> {
    option_env!("RPI_BUILD_TARGET")
}

/// Install-manifest file name, placed next to the rpi executable
/// (ADR-0011 §3).
pub const INSTALL_MANIFEST_FILE_NAME: &str = "rpi.install.json";

/// `rpi.install.json` (ADR-0011 §3). camelCase wire shape (coding-standards
/// §4.4): the install scripts write the same JSON.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallManifest {
    pub version: String,
    /// Full target triple (e.g. `x86_64-unknown-linux-musl`).
    pub target: String,
    /// ISO 8601 UTC timestamp of the install/update.
    pub installed_at: String,
    /// The release asset URL this binary came from.
    pub source_url: String,
    /// sha256 of the downloaded release asset (integrity check value).
    pub sha256: String,
    /// Path of the installed executable.
    pub install_path: String,
    /// Always `"binary"` (ADR-0011 §3).
    pub method: String,
}

impl InstallManifest {
    /// The only manifest method rpi writes (ADR-0011 §3).
    pub const METHOD_BINARY: &'static str = "binary";
}

/// The manifest path for a given executable: same directory, fixed name
/// (ADR-0011 §3 — the CLI locates it via `current_exe()`, the install
/// scripts write it via the install path; one location semantics).
pub fn install_manifest_path_for(exe_path: &Path) -> PathBuf {
    match exe_path.parent() {
        Some(dir) => dir.join(INSTALL_MANIFEST_FILE_NAME),
        None => PathBuf::from(INSTALL_MANIFEST_FILE_NAME),
    }
}

/// Read the manifest next to `exe_path`; a missing or corrupt manifest
/// degrades to "no manifest" (`None`) — self-update stays available and
/// uninstall falls back to manual guidance (ADR-0011 §3).
pub fn read_install_manifest_for(exe_path: &Path) -> Option<InstallManifest> {
    let text = std::fs::read_to_string(install_manifest_path_for(exe_path)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist the manifest next to `exe_path` (atomic write, same crash
/// consistency rationale as settings/trust stores).
pub fn write_install_manifest_for(
    exe_path: &Path,
    manifest: &InstallManifest,
) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    atomic_write(&install_manifest_path_for(exe_path), &text)
}

/// The update instruction shown by the startup banner / new-version
/// notification (T18, ADR-0011 §6): self-updatable installs get
/// `rpi update --self`; a binary build without a target triple (non-cargo
/// build) cannot self-update and gets the download URL directly. The
/// install method is always Binary (D-055), so it takes no method
/// parameter.
pub fn self_update_instruction_for(target: Option<&str>) -> String {
    if target.is_none() {
        return format!("Download the latest release from {SELF_UPDATE_DOWNLOAD_URL}");
    }
    format!("Run {APP_NAME} update --self")
}

/// [`self_update_instruction_for`] for the current installation.
pub fn self_update_instruction() -> String {
    self_update_instruction_for(build_target())
}

/// The data root `rpi self-uninstall --purge` offers to delete (ADR-0011
/// §5): `~/.rpi` at the default layout; when `RPI_CODING_AGENT_DIR`
/// redirects the agent dir elsewhere, the effective agent dir is the
/// deletable root (never delete the parent of a redirected dir).
pub fn get_uninstall_data_dir() -> PathBuf {
    let agent_dir = get_agent_dir();
    if let Some(home) = home_dir() {
        let default_agent_dir = home.join(CONFIG_DIR_NAME).join("agent");
        if agent_dir == default_agent_dir {
            return home.join(CONFIG_DIR_NAME);
        }
    }
    agent_dir
}

// ===== T14-W6a: configurable product endpoints (ADR-0002 §8) =====
//
// The three product HTTP callbacks — version check, install telemetry, and
// the remote model catalog — resolve their endpoint from the environment
// first, then settings, then the built-in default (`https://revpi.dev`). Any
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

/// Rpi-specific (extension-distribution design §7.2): default extension
/// registry base URL; the index endpoint is
/// `<base>/api/extensions/<name>.json` and the download mirror is
/// `<base>/extensions/download/<owner>/<repo>/<tag>/<file>`.
pub const DEFAULT_REGISTRY_URL: &str = "https://revpi.dev";
/// Rpi-specific (extension-distribution design §7.2): override for the
/// extension registry base URL (mirror or test injection; `"off"` disables
/// the registry channel entirely via [`resolve_endpoint`]).
pub const ENV_REGISTRY_URL: &str = "RPI_REGISTRY_URL";

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
        // list: the directory encoding rule covers the Windows drive-letter colon).
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
            resolve_endpoint(None, None, "https://revpi.dev/api"),
            Some("https://revpi.dev/api".to_string())
        );
        // Empty strings fall through to the next level (JS `||` semantics).
        assert_eq!(
            resolve_endpoint(Some(""), Some(""), "https://revpi.dev/api"),
            Some("https://revpi.dev/api".to_string())
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
    //! T18 self-update coverage (ADR-0011): install manifest, build
    //! target, and the update instruction. The upstream install-method
    //! command tests were removed with the npm/pnpm/yarn/bun port (D-055).

    use super::*;

    #[test]
    fn share_viewer_url_default_override_and_empty_fallback() {
        // Single test for all env manipulation — parallel tests would race
        // on `RPI_SHARE_VIEWER_URL`.
        std::env::remove_var(ENV_SHARE_VIEWER_URL);
        assert_eq!(
            get_share_viewer_url("abc123"),
            "https://revpi.dev/session/#abc123"
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
            "https://revpi.dev/session/#abc123"
        );
        std::env::remove_var(ENV_SHARE_VIEWER_URL);
    }

    // ---- T18: install manifest + build target + update instruction ----

    #[test]
    fn test_build_target_is_injected_under_cargo() {
        // build.rs injects RPI_BUILD_TARGET via cargo:rustc-env (ADR-0011
        // §4); under `cargo test` it is always present.
        let target = build_target().expect("RPI_BUILD_TARGET");
        assert!(target.contains('-'), "{target}");
    }

    #[test]
    fn test_install_manifest_roundtrip_and_camel_case_shape() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-manifest-test-{}-{}",
            std::process::id(),
            "roundtrip"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rpi");
        std::fs::write(&exe, b"bin").unwrap();
        assert_eq!(
            install_manifest_path_for(&exe),
            dir.join(INSTALL_MANIFEST_FILE_NAME)
        );
        let manifest = InstallManifest {
            version: "1.2.3".to_string(),
            target: "x86_64-unknown-linux-musl".to_string(),
            installed_at: "2026-08-10T05:00:00.000Z".to_string(),
            source_url: "https://github.com/revpidev/rpi/releases/download/v1.2.3/rpi-1.2.3-x86_64-unknown-linux-musl.tar.gz".to_string(),
            sha256: "ab".to_string(),
            install_path: exe.to_string_lossy().into_owned(),
            method: InstallManifest::METHOD_BINARY.to_string(),
        };
        write_install_manifest_for(&exe, &manifest).unwrap();
        let raw = std::fs::read_to_string(install_manifest_path_for(&exe)).unwrap();
        // G5: camelCase wire shape (the install scripts write the same).
        assert!(raw.contains("\"installedAt\""), "{raw}");
        assert!(raw.contains("\"sourceUrl\""), "{raw}");
        assert!(raw.contains("\"installPath\""), "{raw}");
        assert!(!raw.contains("installed_at"), "{raw}");
        assert_eq!(read_install_manifest_for(&exe), Some(manifest));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_install_manifest_missing_or_corrupt_degrades_to_none() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-manifest-test-{}-{}",
            std::process::id(),
            "corrupt"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rpi");
        // Missing manifest → None.
        assert_eq!(read_install_manifest_for(&exe), None);
        // Corrupt manifest → None (ADR-0011 §3: degrade to "no manifest").
        std::fs::write(install_manifest_path_for(&exe), "not json").unwrap();
        assert_eq!(read_install_manifest_for(&exe), None);
        std::fs::write(install_manifest_path_for(&exe), "{}").unwrap();
        assert_eq!(read_install_manifest_for(&exe), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_self_update_instruction_matches_availability() {
        // T18 (ADR-0011 §6): self-updatable installs get `rpi update
        // --self`; a binary build without a target triple gets the URL.
        // The install method is always Binary (D-055), so the method
        // parameter is gone.
        assert_eq!(
            self_update_instruction_for(Some("x86_64-unknown-linux-gnu")),
            "Run rpi update --self"
        );
        assert_eq!(
            self_update_instruction_for(None),
            format!("Download the latest release from {SELF_UPDATE_DOWNLOAD_URL}")
        );
    }

    #[test]
    fn test_uninstall_data_dir_default_layout() {
        // At the default layout the purge root is `~/.rpi` (the parent of
        // the agent dir); a redirected agent dir deletes itself instead.
        let agent_dir = get_agent_dir();
        let data_dir = get_uninstall_data_dir();
        if let Some(home) = home_dir() {
            if agent_dir == home.join(CONFIG_DIR_NAME).join("agent") {
                assert_eq!(data_dir, home.join(CONFIG_DIR_NAME));
                return;
            }
        }
        assert_eq!(data_dir, agent_dir);
    }
}
