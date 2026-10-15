//! `getToolPath` subset — port of
//! `packages/coding-agent/src/utils/tools-manager.ts:83-102` (T13/T15, #62).
//!
//! Upstream resolves a managed tool from the agent bin directory
//! (`getBinDir()` = `<agentDir>/bin`, config.ts:639-641) first and from the
//! system `PATH` second, returning the bare command name for a `PATH` hit
//! (the caller resolves it again when spawning). rpi has no managed-tool
//! download/install step (`ensureTool`, tools-manager.ts:349-400), so only
//! the lookup half is ported; a missing tool makes the caller fall back to
//! its own path resolution.
//!
//! Shape differences from upstream:
//! - `commandExists` (tools-manager.ts:72-80) probes with `spawnSync(cmd,
//!   ["--version"])`. The port checks `PATH` membership instead: probing
//!   would spawn a process at startup for every candidate, and a
//!   non-runnable hit degrades to the caller's fallback anyway (the `fd`
//!   walk treats a spawn failure as an empty result).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The `fd` command names tried in order (tools-manager.ts:34
/// `systemBinaryNames: ["fd", "fdfind"]`).
pub const FD_BINARY_NAMES: [&str; 2] = ["fd", "fdfind"];

/// Resolve `name` from `bin_dir` first, then from `path_env` (same order as
/// `getToolPath`, tools-manager.ts:83-102). A `bin_dir` hit returns the
/// managed absolute path; a `PATH` hit returns the bare command name
/// (upstream's convention — it is already resolvable by the caller).
pub fn get_tool_path(name: &str, bin_dir: &Path, path_env: Option<&OsStr>) -> Option<String> {
    let managed = bin_dir.join(executable_name(name));
    if managed.is_file() {
        return Some(managed.to_string_lossy().into_owned());
    }
    find_executable(name, path_env).map(|_| name.to_string())
}

/// Platform executable file name (`platform() === "win32"` gets `.exe`,
/// tools-manager.ts:88).
fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// First `PATH` directory containing a runnable `name`.
fn find_executable(name: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    let mut candidates = vec![name.to_string()];
    if let Some(file_name) = Path::new(name).file_name().and_then(|f| f.to_str())
        && file_name == name
        && !name.ends_with(".exe")
    {
        candidates.push(format!("{name}.exe"));
    }
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in &candidates {
            let full = dir.join(candidate);
            if is_executable(&full) {
                return Some(full);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-tool-paths-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[cfg(unix)]
    fn write_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\nexit 0\n").expect("write executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
    }

    #[cfg(not(unix))]
    fn write_executable(path: &Path) {
        std::fs::write(path, "binary").expect("write executable");
    }

    #[test]
    fn managed_bin_dir_wins_over_path() {
        let bin_dir = temp_dir("managed");
        let path_dir = temp_dir("path");
        write_executable(&bin_dir.join(executable_name("fd").as_str()));
        write_executable(&path_dir.join(executable_name("fd").as_str()));

        let path_env = std::env::join_paths([&path_dir]).expect("join paths");
        let resolved =
            get_tool_path("fd", &bin_dir, Some(path_env.as_os_str())).expect("managed fd resolves");
        assert_eq!(
            resolved,
            bin_dir
                .join(executable_name("fd"))
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn path_hit_returns_the_command_name() {
        let bin_dir = temp_dir("empty-managed");
        let path_dir = temp_dir("path-only");
        write_executable(&path_dir.join(executable_name("fd").as_str()));

        let path_env = std::env::join_paths([&path_dir]).expect("join paths");
        assert_eq!(
            get_tool_path("fd", &bin_dir, Some(path_env.as_os_str())),
            Some("fd".to_string())
        );
        // The `fdfind` alias (Debian/Ubuntu package name).
        write_executable(&path_dir.join(executable_name("fdfind").as_str()));
        assert_eq!(
            get_tool_path("fdfind", &bin_dir, Some(path_env.as_os_str())),
            Some("fdfind".to_string())
        );
    }

    #[test]
    fn missing_tool_returns_none() {
        let bin_dir = temp_dir("missing-managed");
        let path_dir = temp_dir("missing-path");
        let path_env = std::env::join_paths([&path_dir]).expect("join paths");
        assert_eq!(
            get_tool_path("fd", &bin_dir, Some(path_env.as_os_str())),
            None
        );
        assert_eq!(get_tool_path("fd", &bin_dir, None), None);
    }

    /// A plain file on `PATH` is not a hit on Unix (no executable bit);
    /// Windows has no such distinction, so the case is Unix-only.
    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_not_a_hit() {
        let bin_dir = temp_dir("non-exec-managed");
        let path_dir = temp_dir("non-exec-path");
        std::fs::write(path_dir.join(executable_name("fd")), "not executable").expect("write");
        let path_env = std::env::join_paths([&path_dir]).expect("join paths");
        assert_eq!(
            get_tool_path("fd", &bin_dir, Some(path_env.as_os_str())),
            None
        );
    }

    /// #62 test plan: the `fd` lookup tries the Debian alias after `fd`.
    #[test]
    fn fd_binary_names_match_upstream() {
        assert_eq!(FD_BINARY_NAMES, ["fd", "fdfind"]);
    }
}
