//! Port of the gist-share slice of `handleShareCommand`
//! (`packages/coding-agent/src/modes/interactive/interactive-mode.ts`:
//! 5511-5603) @ pi 0.82.1 (2efa728): `gh auth status` + `gh gist create
//! --public=false` behind an injectable runner (the W2
//! `PackageCommandRunner` pattern), so tests never touch real processes.
//!
//! Intentional differences (vs upstream):
//! - Upstream `spawnSync`/`spawn` calls are replaced by the [`ShareRunner`]
//!   trait; the process wiring lives in [`SystemShareRunner`].
//! - Cancellation: upstream kills the child via `proc.kill()` from the
//!   loader's `onAbort`; here the caller passes an [`AtomicBool`] that
//!   [`SystemShareRunner::gist_create`] polls (50ms) and kills the child on.
//! - The viewer URL lives in `config.rs` (`get_share_viewer_url`), the single
//!   place that reads `PIR_SHARE_VIEWER_URL` — W6 endpoint configurability
//!   hooks in there.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Poll interval while waiting on `gh gist create` (cancellation latency).
const GIST_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Result of `gh auth status` (interactive-mode.ts:5513-5522).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhAuthStatus {
    /// Exit code 0.
    Ok,
    /// Spawned but exited non-zero → "not logged in".
    NotLoggedIn,
    /// Spawn failed → gh not installed.
    NotInstalled,
}

/// Outcome of `gh gist create --public=false <file>`
/// (interactive-mode.ts:5562-5574: `{ stdout, stderr, code }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GistCreateOutcome {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Injectable gh runner (the W2 `PackageCommandRunner` pattern).
pub trait ShareRunner: Send + Sync {
    /// `spawnSync("gh", ["auth", "status"])` (interactive-mode.ts:5514).
    fn auth_status(&self) -> GhAuthStatus;
    /// `spawn("gh", ["gist", "create", "--public=false", file])`
    /// (interactive-mode.ts:5563). Blocks until the process exits; when
    /// `cancelled` is set the child is killed (upstream `proc.kill()`,
    /// interactive-mode.ts:5551).
    fn gist_create(&self, file: &Path, cancelled: Arc<AtomicBool>) -> GistCreateOutcome;
}

/// Default runner: real `gh` processes via `std::process`.
pub struct SystemShareRunner;

impl ShareRunner for SystemShareRunner {
    fn auth_status(&self) -> GhAuthStatus {
        match std::process::Command::new("gh")
            .args(["auth", "status"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => GhAuthStatus::Ok,
            Ok(_) => GhAuthStatus::NotLoggedIn,
            Err(_) => GhAuthStatus::NotInstalled,
        }
    }

    fn gist_create(&self, file: &Path, cancelled: Arc<AtomicBool>) -> GistCreateOutcome {
        let spawn = std::process::Command::new("gh")
            .args(["gist", "create", "--public=false"])
            .arg(file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match spawn {
            Ok(child) => child,
            Err(error) => {
                // Spawn failure after a successful auth check — surface it
                // like a non-zero exit with the OS error as stderr.
                return GistCreateOutcome {
                    code: None,
                    stdout: String::new(),
                    stderr: error.to_string(),
                };
            }
        };
        loop {
            if cancelled.load(Ordering::Relaxed) {
                let _ = child.kill();
            }
            match child.try_wait() {
                // Waited: collect piped output.
                Ok(Some(_)) => {
                    return match child.wait_with_output() {
                        Ok(output) => GistCreateOutcome {
                            code: output.status.code(),
                            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        },
                        Err(error) => GistCreateOutcome {
                            code: None,
                            stdout: String::new(),
                            stderr: error.to_string(),
                        },
                    };
                }
                // Still running.
                Ok(None) => std::thread::sleep(GIST_POLL_INTERVAL),
                Err(error) => {
                    return GistCreateOutcome {
                        code: None,
                        stdout: String::new(),
                        stderr: error.to_string(),
                    };
                }
            }
        }
    }
}

/// Remove the temporary export file after a share settles, plus its
/// `pir-share-*` parent directory (created per-invocation by
/// `handle_share_command`; best-effort, `remove_dir` refuses non-empty
/// dirs so an unexpected parent is never deleted).
pub fn cleanup_share_tmp_file(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        if parent
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("pir-share-"))
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Restrict the share temp file/directory to the current user (T14
/// review): the exported session HTML can contain private conversation
/// content; on multi-user machines the default 0644/0755 umask-derived
/// modes would leave it world-readable in /tmp (upstream parity residual —
/// upstream's fixed `os.tmpdir()/session.html` has the same exposure).
/// Best-effort on unix; no-op elsewhere.
pub fn restrict_share_tmp_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        if let Some(parent) = path.parent() {
            if parent
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("pir-share-"))
            {
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Extract the gist ID from the URL printed by `gh gist create`
/// (interactive-mode.ts:5585-5591: `gistUrl?.split("/").pop()`, empty →
/// parse failure).
pub fn parse_gist_id(gist_url: &str) -> Option<&str> {
    gist_url
        .trim()
        .rsplit('/')
        .next()
        .filter(|gist_id| !gist_id.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_gist_id_from_url() {
        assert_eq!(
            parse_gist_id("https://gist.github.com/user/abc123"),
            Some("abc123")
        );
        assert_eq!(
            parse_gist_id("  https://gist.github.com/user/abc123\n"),
            Some("abc123")
        );
        assert_eq!(parse_gist_id("abc123"), Some("abc123"));
        // Empty stdout → "" → parse failure (interactive-mode.ts:5588-5590).
        assert_eq!(parse_gist_id(""), None);
        assert_eq!(parse_gist_id("   "), None);
        // Trailing slash: upstream `split("/").pop()` yields "" → failure.
        assert_eq!(parse_gist_id("https://gist.github.com/user/abc123/"), None);
    }
}
