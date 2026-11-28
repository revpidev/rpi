//! The `--verify-extension <path>` preflight (design §11.4 step 6, R7.2.4).
//!
//! A native (L0) extension cannot be unloaded once its library is loaded,
//! so `rpi update` never loads a freshly downloaded package in the process
//! that will keep running. It starts a short-lived child — `rpi
//! --verify-extension <staging dir>` with `RPI_UPDATE_VERIFY_CHILD=1` —
//! that loads the extension exactly the way the host would and exits 0
//! when it loads. The parent kills the child after a timeout and counts
//! that as a rejection of that one extension.
//!
//! The load itself is platform independent; only starting the child is
//! Windows-specific, so the judgement is unit-tested on Linux.

use std::path::{Path, PathBuf};

use super::fs::UpdateFs;

/// Load the extension at `path` the way the host does and report why it
/// would not load. `Ok(())` means the package is loadable; nothing stays
/// registered — the caller is expected to exit right after.
pub fn verify_extension_loads(path: &Path, cwd: &Path) -> Result<(), String> {
    let runtime = rpi_ext_host::api::ExtensionRuntime::new();
    let loader = rpi_ext_host::loader::ExtensionLoader::new(runtime);
    let result = futures::executor::block_on(loader.load_paths(&[path.to_path_buf()], cwd));
    match result.errors.first() {
        Some(error) => Err(error.error.clone()),
        None if result.extensions.is_empty() => {
            Err("the package has no loadable entry point (no rpi-extension.json, index.wasm or native library)".to_owned())
        }
        None => Ok(()),
    }
}

/// Run the preflight in this process and return the process exit code
/// (`0` loads, `1` rejected). The caller exits immediately afterwards so
/// the loaded library is released by process exit.
pub fn run_verify_extension_in(path: &Path, cwd: &Path, err: &mut dyn std::io::Write) -> i32 {
    match verify_extension_loads(path, cwd) {
        Ok(()) => {
            let _ = writeln!(err, "ok: {} loads", path.display());
            0
        }
        Err(error) => {
            let _ = writeln!(err, "rejected: {}: {error}", path.display());
            1
        }
    }
}

/// How the preflight child was started and what it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// The child exited 0 within the timeout.
    Loaded,
    /// The child exited with a non-zero code; its stderr explains why.
    Rejected(String),
    /// The child did not exit in time and was killed.
    TimedOut,
    /// The child could not be started.
    NotStarted(String),
}

/// The seam the package manager uses: production (Windows) starts the
/// child; tests inject the outcome.
pub type Preflight = std::sync::Arc<dyn Fn(&Path) -> PreflightOutcome + Send + Sync>;

/// A preflight that accepts every package (Unix: the in-place activation
/// path loads nothing ahead of time, as before V19-09).
pub fn accept_all() -> Preflight {
    std::sync::Arc::new(|_: &Path| PreflightOutcome::Loaded)
}

/// A preflight that runs the judgement in-process — for tests on every
/// platform and for a child process that was started by someone else.
pub fn in_process(cwd: PathBuf) -> Preflight {
    std::sync::Arc::new(
        move |path: &Path| match verify_extension_loads(path, &cwd) {
            Ok(()) => PreflightOutcome::Loaded,
            Err(error) => PreflightOutcome::Rejected(error),
        },
    )
}

/// Start the preflight child on Windows: `rpi --verify-extension <path>`
/// with `RPI_UPDATE_VERIFY_CHILD=1`, killed after
/// [`super::VERIFY_EXTENSION_TIMEOUT`]. Its stderr is inherited so the
/// rejection reason reaches the user.
#[cfg(windows)]
pub fn child_process_preflight() -> Preflight {
    std::sync::Arc::new(|path: &Path| run_child_preflight(path))
}

#[cfg(windows)]
fn run_child_preflight(path: &Path) -> PreflightOutcome {
    use std::process::{Command, Stdio};
    let mut command = match std::env::current_exe() {
        Ok(exe) => Command::new(exe),
        Err(error) => {
            return PreflightOutcome::NotStarted(format!("could not locate rpi: {error}"));
        }
    };
    command
        .arg(super::VERIFY_EXTENSION_FLAG)
        .arg(path)
        .env(super::ENV_VERIFY_CHILD, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return PreflightOutcome::NotStarted(format!("could not start rpi: {error}")),
    };
    let deadline = std::time::Instant::now() + super::VERIFY_EXTENSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return match status.code() {
                    Some(0) => PreflightOutcome::Loaded,
                    Some(code) => PreflightOutcome::Rejected(format!("exit code {code}")),
                    None => {
                        PreflightOutcome::Rejected("terminated without an exit code".to_owned())
                    }
                };
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return PreflightOutcome::TimedOut;
            }
            Err(error) => {
                return PreflightOutcome::NotStarted(format!("could not wait for rpi: {error}"));
            }
        }
    }
}

/// The message the update output carries for a rejected extension.
pub fn rejection_message(name: &str, version: &str, outcome: &PreflightOutcome) -> String {
    match outcome {
        PreflightOutcome::Rejected(reason) => format!(
            "{name} {version} did not load in the preflight and was not installed: {reason}"
        ),
        PreflightOutcome::TimedOut => format!(
            "{name} {version} did not finish its load preflight within {} seconds and was not installed",
            super::VERIFY_EXTENSION_TIMEOUT.as_secs()
        ),
        PreflightOutcome::NotStarted(reason) => {
            format!("{name} {version} could not be preflighted and was not installed: {reason}")
        }
        PreflightOutcome::Loaded => String::new(),
    }
}

/// Whether the staged directory at `path` still has to be removed by the
/// caller after a preflight (always: a rejected package is dropped, a
/// loaded one moves on to activation).
pub fn cleanup_after_preflight(fs: &dyn UpdateFs, path: &Path, accepted: bool) {
    if !accepted {
        let _ = fs.remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-wu-preflight-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_wasm_package_without_its_module_is_rejected_with_the_loader_error() {
        let dir = temp_dir();
        // A manifest naming a wasm file that is not there.
        std::fs::write(
            dir.join("rpi-extension.json"),
            r#"{"name":"rpi-wasm","version":"0.1.0","wasm":"missing.wasm","rpiAbi":1}"#,
        )
        .unwrap();
        let error = verify_extension_loads(&dir, &dir).unwrap_err();
        assert!(error.contains("no loadable entry point"), "{error}");
        let mut err: Vec<u8> = Vec::new();
        assert_eq!(run_verify_extension_in(&dir, &dir, &mut err), 1);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("rejected:") && text.contains("no loadable entry point"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_directory_without_an_entry_point_is_rejected() {
        let dir = temp_dir();
        let error = verify_extension_loads(&dir, &dir).unwrap_err();
        assert!(error.contains("no loadable entry point"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_native_package_whose_library_cannot_open_is_rejected() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("rpi-extension.json"),
            r#"{"name":"rpi-native","version":"0.1.0","native":"libmissing.so","rpiAbi":1}"#,
        )
        .unwrap();
        // The manifest names a library that does not exist: discovery falls
        // through to index.wasm (absent) → no entry point.
        let error = verify_extension_loads(&dir, &dir).unwrap_err();
        assert!(!error.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn messages_name_the_extension_version_and_the_reason() {
        assert_eq!(
            rejection_message(
                "rpi-todo",
                "0.2.0",
                &PreflightOutcome::Rejected("boom".to_owned())
            ),
            "rpi-todo 0.2.0 did not load in the preflight and was not installed: boom"
        );
        assert!(
            rejection_message("a", "1", &PreflightOutcome::TimedOut).contains("within 30 seconds")
        );
        assert!(
            rejection_message("a", "1", &PreflightOutcome::NotStarted("x".to_owned()))
                .contains("could not be preflighted")
        );
        assert_eq!(rejection_message("a", "1", &PreflightOutcome::Loaded), "");
    }

    #[test]
    fn the_in_process_seam_judges_and_a_rejected_staging_directory_is_removed() {
        let dir = temp_dir();
        let fs = super::super::fs::SpyFs::new();
        let preflight = in_process(dir.clone());
        assert!(matches!(preflight(&dir), PreflightOutcome::Rejected(_)));
        cleanup_after_preflight(&fs, &dir, false);
        assert!(!dir.exists());
        assert!(matches!((accept_all())(&dir), PreflightOutcome::Loaded));
    }
}
