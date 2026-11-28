//! On-disk layout of the Windows update state (design §11.2).
//!
//! The executable prefix (`%LOCALAPPDATA%\Programs\rpi`) and the agent
//! directory (`~/.rpi`) are not assumed to share a volume: the executable
//! and its state files stage next to the executable, extensions stage under
//! their own install root, and every replace is a same-volume rename.

use std::path::{Path, PathBuf};

/// Directory next to the executable holding all update state.
pub const UPDATE_DIR_NAME: &str = ".update";
/// Staged executable directory: `.update/staging/<version>/`.
pub const STAGING_DIR_NAME: &str = "staging";
pub const PENDING_FILE: &str = "pending.json";
pub const JOURNAL_FILE: &str = "journal.json";
pub const UNDO_FILE: &str = "undo.json";
pub const RESULT_FILE: &str = "update-result.json";
pub const BLOCKED_FILE: &str = "blocked.json";
/// `.update/boot/<pid>.json`: first-boot records.
pub const BOOT_DIR_NAME: &str = "boot";
/// OS-level exclusive lock held by a running startup transaction.
pub const LOCK_FILE: &str = ".update.lock";
/// Extension staging root under an extensions root (same volume).
pub const EXTENSION_STAGING_DIR: &str = ".staging";
/// Process registrations under the global extensions root.
pub const EXTENSION_PINS_DIR: &str = ".pins";
/// Old-image file left by the startup transaction: `rpi.exe.old-<version>`.
pub const OLD_EXE_SUFFIX: &str = ".old-";
/// New image set aside by a rollback: `rpi.exe.bad-<version>`.
pub const BAD_EXE_SUFFIX: &str = ".bad-";
pub const UPDATE_LOG_FILE: &str = "update.log";
/// Rotated log: `update.log.1` (one generation, 1 MiB cap).
pub const UPDATE_LOG_ROTATED_SUFFIX: &str = ".1";
pub const UPDATE_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Every path the update machinery touches for one installed executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePaths {
    /// The executable as it was resolved when the process started (the
    /// original path: renaming the running image does not change it).
    pub exe_path: PathBuf,
    /// `<exe prefix>/.update`.
    pub update_dir: PathBuf,
    /// `<exe prefix>/rpi.install.json` (schema unchanged, ADR-0011 §3).
    pub manifest_path: PathBuf,
    /// `%LOCALAPPDATA%\rpi\logs\update.log`.
    pub log_path: PathBuf,
    /// Extension install roots: `<agent_dir>/extensions` and, when known,
    /// `<cwd>/.rpi/extensions`. Pins live under the first (global) one.
    pub extension_roots: Vec<PathBuf>,
}

impl UpdatePaths {
    /// Paths for `exe_path`. `cwd` adds the project-local extension root.
    pub fn new(exe_path: &Path, log_path: PathBuf, agent_dir: &Path, cwd: Option<&Path>) -> Self {
        let prefix = exe_path.parent().unwrap_or_else(|| Path::new("."));
        let mut extension_roots = vec![agent_dir.join("extensions")];
        if let Some(cwd) = cwd {
            let project = crate::config::get_project_config_dir(cwd).join("extensions");
            if !extension_roots.contains(&project) {
                extension_roots.push(project);
            }
        }
        UpdatePaths {
            exe_path: exe_path.to_path_buf(),
            update_dir: prefix.join(UPDATE_DIR_NAME),
            manifest_path: crate::config::install_manifest_path_for(exe_path),
            log_path,
            extension_roots,
        }
    }

    /// Paths for the running process: the executable, the agent dir, the
    /// current directory (project-local extensions) and the log file —
    /// `%LOCALAPPDATA%\rpi\logs\update.log`, or `update.log` inside the
    /// update directory when there is no user profile (logging is never
    /// fatal either way).
    pub fn for_exe(exe_path: &Path, agent_dir: &Path, cwd: Option<&Path>) -> Self {
        let prefix = exe_path.parent().unwrap_or_else(|| Path::new("."));
        let log_path = default_log_path()
            .unwrap_or_else(|| prefix.join(UPDATE_DIR_NAME).join(UPDATE_LOG_FILE));
        UpdatePaths::new(exe_path, log_path, agent_dir, cwd)
    }

    /// The global (agent) extensions root; pins are registered there.
    pub fn global_extension_root(&self) -> &Path {
        &self.extension_roots[0]
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.update_dir.join(STAGING_DIR_NAME)
    }

    /// `.update/staging/<version>/`.
    pub fn binary_staging_dir(&self, version: &str) -> PathBuf {
        self.staging_dir().join(safe_version_component(version))
    }

    pub fn pending_path(&self) -> PathBuf {
        self.update_dir.join(PENDING_FILE)
    }

    pub fn journal_path(&self) -> PathBuf {
        self.update_dir.join(JOURNAL_FILE)
    }

    pub fn undo_path(&self) -> PathBuf {
        self.update_dir.join(UNDO_FILE)
    }

    pub fn result_path(&self) -> PathBuf {
        self.update_dir.join(RESULT_FILE)
    }

    pub fn blocked_path(&self) -> PathBuf {
        self.update_dir.join(BLOCKED_FILE)
    }

    pub fn boot_dir(&self) -> PathBuf {
        self.update_dir.join(BOOT_DIR_NAME)
    }

    /// `.update/boot/<pid>.json`.
    pub fn boot_record_path(&self, pid: u32) -> PathBuf {
        self.boot_dir().join(format!("{pid}.json"))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.update_dir.join(LOCK_FILE)
    }

    pub fn pins_dir(&self) -> PathBuf {
        self.global_extension_root().join(EXTENSION_PINS_DIR)
    }

    /// `<extensions root>/.staging`.
    pub fn extension_staging_dir(&self, root: &Path) -> PathBuf {
        root.join(EXTENSION_STAGING_DIR)
    }

    /// `.staging/<name>-<version>-<pid>`: the extraction directory of one
    /// extension about to be placed as a generation.
    pub fn extension_staging_path(root: &Path, name: &str, version: &str, pid: u32) -> PathBuf {
        root.join(EXTENSION_STAGING_DIR)
            .join(format!("{name}-{}-{pid}", safe_version_component(version)))
    }

    /// The executable file name (`rpi.exe`).
    pub fn exe_file_name(&self) -> String {
        self.exe_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// `rpi.exe.old-<version>`: where the startup transaction parks the
    /// running image.
    pub fn old_exe_path(&self, version: &str) -> PathBuf {
        self.sibling(&format!(
            "{}{OLD_EXE_SUFFIX}{}",
            self.exe_file_name(),
            safe_version_component(version)
        ))
    }

    /// `rpi.exe.bad-<version>`: where a rollback parks the failed image.
    pub fn bad_exe_path(&self, version: &str) -> PathBuf {
        self.sibling(&format!(
            "{}{BAD_EXE_SUFFIX}{}",
            self.exe_file_name(),
            safe_version_component(version)
        ))
    }

    /// A sibling of the executable that does not exist yet, falling back to
    /// a `.t<unix seconds>` suffix when the preferred name is taken. The
    /// marker keeps the version readable by
    /// [`version_from_exe_leftover_name`].
    pub fn unused_sibling(&self, preferred: PathBuf) -> PathBuf {
        if !preferred.exists() {
            return preferred;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let mut candidate = preferred.clone().into_os_string();
        candidate.push(format!(".t{stamp}"));
        PathBuf::from(candidate)
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.exe_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(name)
    }
}

/// A version string reduced to characters that are safe in a file name.
pub fn safe_version_component(version: &str) -> String {
    let cleaned: String = version
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    // `.` and `..` would name other directories; the reader-side guard
    // (`is_safe_generation_path`) rejects them, the writer never produces
    // them.
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "_".to_owned()
    } else {
        cleaned
    }
}

/// The version encoded in an old/bad executable name
/// (`rpi.exe.old-<version>`), when `file_name` carries either suffix.
pub fn version_from_exe_leftover_name(exe_file_name: &str, file_name: &str) -> Option<String> {
    let rest = file_name.strip_prefix(exe_file_name)?;
    let rest = rest
        .strip_prefix(OLD_EXE_SUFFIX)
        .or_else(|| rest.strip_prefix(BAD_EXE_SUFFIX))?;
    // A timestamp fallback suffix (`.t<unix seconds>`) is not part of the version.
    let version = match rest.rsplit_once(".t") {
        Some((version, stamp))
            if !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()) =>
        {
            version
        }
        _ => rest,
    };
    Some(version.to_owned())
}

/// `%LOCALAPPDATA%\rpi\logs\update.log`; `None` when `LOCALAPPDATA` is unset
/// (no user profile), in which case logging is disabled and nothing else
/// changes (design §11.10: a log failure is never fatal).
pub fn default_log_path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty())?;
    Some(
        PathBuf::from(local)
            .join(crate::config::APP_NAME)
            .join("logs")
            .join(UPDATE_LOG_FILE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Separator-independent expectations: the same strings parse into the
    /// same parent/child relations on every platform.
    fn joins(parts: &[&str]) -> PathBuf {
        let mut path = PathBuf::new();
        for part in parts {
            path.push(part);
        }
        path
    }

    #[test]
    fn paths_hang_off_the_executable_prefix() {
        let exe = joins(&["/", "data", "Programs", "rpi", "rpi.exe"]);
        let log = joins(&["/", "data", "rpi", "logs", "update.log"]);
        let agent = joins(&["/", "home", "me", ".rpi", "agent"]);
        let cwd = joins(&["/", "work", "project"]);
        let paths = UpdatePaths::new(&exe, log, &agent, Some(&cwd));
        assert_eq!(
            paths.update_dir,
            joins(&["/", "data", "Programs", "rpi", ".update"])
        );
        assert_eq!(
            paths.binary_staging_dir("0.1.9-rc.4"),
            joins(&[
                "/",
                "data",
                "Programs",
                "rpi",
                ".update",
                "staging",
                "0.1.9-rc.4"
            ])
        );
        assert_eq!(
            paths.manifest_path,
            joins(&["/", "data", "Programs", "rpi", "rpi.install.json"])
        );
        assert_eq!(
            paths.extension_roots,
            vec![
                joins(&["/", "home", "me", ".rpi", "agent", "extensions"]),
                joins(&["/", "work", "project", ".rpi", "extensions"]),
            ]
        );
        assert_eq!(
            paths.pins_dir(),
            joins(&["/", "home", "me", ".rpi", "agent", "extensions", ".pins"])
        );
        assert_eq!(
            paths.old_exe_path("0.1.9-rc.3"),
            joins(&["/", "data", "Programs", "rpi", "rpi.exe.old-0.1.9-rc.3"])
        );
        assert_eq!(
            paths.bad_exe_path("0.1.9-rc.4"),
            joins(&["/", "data", "Programs", "rpi", "rpi.exe.bad-0.1.9-rc.4"])
        );
        assert_eq!(
            paths.boot_record_path(42),
            joins(&["/", "data", "Programs", "rpi", ".update", "boot", "42.json"])
        );
        assert_eq!(
            paths.lock_path(),
            joins(&["/", "data", "Programs", "rpi", ".update", ".update.lock"])
        );
        assert_eq!(
            UpdatePaths::extension_staging_path(
                &paths.extension_roots[0],
                "rpi-todo",
                "0.1.9-rc.4",
                7
            ),
            joins(&[
                "/",
                "home",
                "me",
                ".rpi",
                "agent",
                "extensions",
                ".staging",
                "rpi-todo-0.1.9-rc.4-7"
            ])
        );
        assert_eq!(paths.exe_file_name(), "rpi.exe");
    }

    #[test]
    fn project_root_is_deduplicated_and_optional() {
        let agent_dir = Path::new("/home/me/.rpi/agent");
        let without_cwd = UpdatePaths::new(
            Path::new("/bin/rpi"),
            PathBuf::from("/log"),
            agent_dir,
            None,
        );
        assert_eq!(without_cwd.extension_roots.len(), 1);
        // `RPI_CODING_AGENT_DIR=<cwd>/.rpi`: the two roots are one and the same.
        let same = UpdatePaths::new(
            Path::new("/bin/rpi"),
            PathBuf::from("/log"),
            Path::new("/work/project/.rpi"),
            Some(Path::new("/work/project")),
        );
        assert_eq!(same.extension_roots.len(), 1);
        assert_eq!(
            same.global_extension_root(),
            Path::new("/work/project/.rpi/extensions")
        );
    }

    #[test]
    fn version_components_are_file_name_safe() {
        assert_eq!(safe_version_component("0.1.9-rc.4"), "0.1.9-rc.4");
        assert_eq!(safe_version_component(" 1.2.3+build "), "1.2.3+build");
        assert_eq!(safe_version_component("../evil:x"), ".._evil_x");
        assert_eq!(safe_version_component("."), "_");
        assert_eq!(safe_version_component(".."), "_");
        assert_eq!(safe_version_component(""), "_");
    }

    #[test]
    fn leftover_names_round_trip_their_version() {
        assert_eq!(
            version_from_exe_leftover_name("rpi.exe", "rpi.exe.old-0.1.9-rc.3").as_deref(),
            Some("0.1.9-rc.3")
        );
        assert_eq!(
            version_from_exe_leftover_name("rpi.exe", "rpi.exe.bad-0.1.9-rc.4.t1700000000")
                .as_deref(),
            Some("0.1.9-rc.4")
        );
        assert_eq!(
            version_from_exe_leftover_name("rpi.exe", "rpi.exe.old-0.1.9").as_deref(),
            Some("0.1.9")
        );
        assert_eq!(version_from_exe_leftover_name("rpi.exe", "rpi.exe"), None);
        assert_eq!(
            version_from_exe_leftover_name("rpi.exe", "other.exe.old-1"),
            None
        );
    }

    #[test]
    fn unused_sibling_falls_back_to_a_stamp() {
        let temp = std::env::temp_dir().join(format!("rpi-layout-{}", std::process::id()));
        std::fs::create_dir_all(temp.join(".update")).unwrap();
        std::fs::write(temp.join("rpi.exe.old-1.0.0"), b"x").unwrap();
        let paths = UpdatePaths::new(&temp.join("rpi.exe"), temp.join("log"), &temp, None);
        let taken = temp.join("rpi.exe.old-1.0.0");
        let chosen = paths.unused_sibling(taken.clone());
        assert_ne!(chosen, taken);
        assert!(
            chosen
                .to_string_lossy()
                .starts_with(taken.to_string_lossy().as_ref())
        );
        let free = paths.unused_sibling(temp.join("rpi.exe.old-2.0.0"));
        assert_eq!(free, temp.join("rpi.exe.old-2.0.0"));
        let _ = std::fs::remove_dir_all(temp);
    }
}
