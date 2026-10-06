//! Plan file derivation and atomic writes (TE43 FR-D; plugin 02 §5).
//!
//! Path: `<cwd>/.rpi/plans/<session-id>-<n>.md` (or the configured
//! `planDir` root). The session component is sanitized so the id can
//! never escape the plan directory; `<n>` starts at 1 and increments past
//! every existing `<session-id>-<n>.md` file. The directory is created
//! lazily on first write, and allocation goes through `create_new` with a
//! retry loop so concurrent writers cannot collide on one number.
//!
//! `write_plan` has no path parameter (01 §5 R-PM-4.2): the path is always
//! derived here from the bound session, so an injected path is
//! structurally impossible.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::DEFAULT_PLAN_DIR;

/// The session key used in the plan filename: the session id with any
/// character outside `[A-Za-z0-9._-]` replaced by `-`; an empty session id
/// (unbound host) reads as `anonymous`.
pub fn session_key(session_id: &str) -> String {
    if session_id.is_empty() {
        return "anonymous".to_owned();
    }
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Expand a leading `~` (alone or `~/...`) with the user's home directory
/// (v0.1.6 review round 2). `~user` forms are left untouched (a relative
/// path), matching the mcp-adapter `~/` convention.
fn expand_tilde(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed == "~" {
        return crate::config::home_dir();
    }
    trimmed
        .strip_prefix("~/")
        .and_then(|rest| crate::config::home_dir().map(|home| home.join(rest)))
}

/// Resolve the plan directory: an absolute `planDir` override is used
/// verbatim; `~`/`~/...` expands against the user's home directory; a
/// relative one (and the default `.rpi/plans`) resolves against the session
/// cwd.
pub fn plan_dir(cwd: &Path, plan_dir: Option<&str>) -> PathBuf {
    match plan_dir {
        Some(dir) if !dir.trim().is_empty() => {
            if let Some(expanded) = expand_tilde(dir) {
                return expanded;
            }
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        }
        _ => cwd.join(DEFAULT_PLAN_DIR),
    }
}

/// Scan `dir` for `<key>-<n>.md` files and return the next free index
/// (at least 1). A missing/unreadable directory reads as 1.
pub fn next_index(dir: &Path, key: &str) -> u32 {
    let prefix = format!("{key}-");
    let mut max = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 1;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(number) = rest.strip_suffix(".md") else {
            continue;
        };
        if let Ok(number) = number.parse::<u32>() {
            max = max.max(number);
        }
    }
    max.saturating_add(1).max(1)
}

/// The next plan path without touching the filesystem.
pub fn next_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}-{}.md", next_index(dir, key)))
}

/// Write `content` to `path` (creating parent directories), returning the
/// byte count. Revisions go through an exclusively created temp file plus
/// rename so a crash cannot leave a torn plan (v0.1.6 review P3; the temp
/// also uses `create_new` like the OAuth/approval writers), and the parent
/// directory is synced best-effort after the rename so the replacement
/// itself is durable.
pub fn write_file(path: &Path, content: &str) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp = path.with_extension(format!("md.{}.{unique}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    sync_parent_dir(path);
    Ok(content.len())
}

/// Best-effort parent-directory sync after a rename (the file data was
/// already flushed); a failure here cannot corrupt the plan, so it is
/// ignored. Windows cannot open a directory as a file, so it is unix-only.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Allocate the next plan file for `key` and write `content` into it.
/// `create_new` makes the allocation atomic; a lost race retries with the
/// next index. Returns the written path and byte count.
pub fn allocate_and_write(
    dir: &Path,
    key: &str,
    content: &str,
) -> std::io::Result<(PathBuf, usize)> {
    std::fs::create_dir_all(dir)?;
    let mut index = next_index(dir, key);
    loop {
        let path = dir.join(format!("{key}-{index}.md"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(content.as_bytes())?;
                return Ok((path, content.len()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                index = index.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let dir = std::env::temp_dir().join(format!(
                "rpi-plan-mode-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn session_key_sanitizes_and_defaults() {
        assert_eq!(session_key("abc-123"), "abc-123");
        assert_eq!(session_key("a/b\\c d"), "a-b-c-d");
        assert_eq!(session_key(""), "anonymous");
        assert_eq!(session_key("weird:name*"), "weird-name-");
    }

    #[test]
    fn plan_dir_resolves_relative_and_absolute_overrides() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            plan_dir(cwd, None),
            PathBuf::from("/work/project/.rpi/plans")
        );
        assert_eq!(
            plan_dir(cwd, Some("plans")),
            PathBuf::from("/work/project/plans")
        );
        assert_eq!(
            plan_dir(cwd, Some("/tmp/plans")),
            PathBuf::from("/tmp/plans")
        );
        assert_eq!(
            plan_dir(cwd, Some("  ")),
            PathBuf::from("/work/project/.rpi/plans")
        );
    }

    #[test]
    fn plan_dir_expands_a_leading_tilde() {
        let Some(home) = crate::config::home_dir() else {
            return;
        };
        let cwd = Path::new("/work/project");
        assert_eq!(plan_dir(cwd, Some("~")), home);
        assert_eq!(plan_dir(cwd, Some("~/plans")), home.join("plans"));
        // `~user` is not a home reference and stays relative.
        assert_eq!(
            plan_dir(cwd, Some("~user/plans")),
            PathBuf::from("/work/project/~user/plans")
        );
    }

    #[test]
    fn index_scans_existing_files_and_ignores_other_sessions() {
        let temp = TempDir::new("index");
        let dir = temp.0.clone();
        assert_eq!(next_index(&dir, "s1"), 1, "missing dir reads as 1");
        std::fs::write(dir.join("s1-1.md"), "a").expect("write");
        std::fs::write(dir.join("s1-2.md"), "b").expect("write");
        std::fs::write(dir.join("s2-9.md"), "c").expect("write");
        std::fs::write(dir.join("s1-x.md"), "d").expect("write");
        assert_eq!(next_index(&dir, "s1"), 3);
        assert_eq!(next_index(&dir, "s2"), 10);
        assert_eq!(
            next_path(&dir, "s1"),
            dir.join("s1-3.md"),
            "next_path does not create"
        );
    }

    #[test]
    fn allocate_and_write_creates_dirs_and_increments() {
        let temp = TempDir::new("alloc");
        let dir = temp.0.join("nested/plans");
        let (first, bytes) = allocate_and_write(&dir, "s1", "plan one").expect("first");
        assert_eq!(first, dir.join("s1-1.md"));
        assert_eq!(bytes, "plan one".len());
        assert_eq!(std::fs::read_to_string(&first).expect("read"), "plan one");
        let (second, _) = allocate_and_write(&dir, "s1", "plan two").expect("second");
        assert_eq!(second, dir.join("s1-2.md"));
    }

    #[test]
    fn write_file_overwrites_the_current_plan() {
        let temp = TempDir::new("overwrite");
        let path = temp.0.join("plans/s1-1.md");
        write_file(&path, "v1").expect("first");
        let bytes = write_file(&path, "v2 longer").expect("second");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "v2 longer");
        assert_eq!(bytes, "v2 longer".len());
    }

    /// v0.1.6 review round 2 (O5): the revision write must keep the
    /// previous plan when it fails. A read-only plan directory makes
    /// `create_new` fail (the old direct `File::create` on the existing
    /// file would still have overwritten it), so this is a true
    /// revert-the-source test. Environments that ignore directory
    /// permission bits (root, some filesystems) are detected with a canary
    /// write and skipped.
    #[cfg(unix)]
    #[test]
    fn failed_revision_write_keeps_the_previous_plan() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("revision-failure");
        let dir = temp.0.join("plans");
        let path = dir.join("s1-1.md");
        write_file(&path, "v1").expect("first");
        write_file(&path, "v2").expect("second");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555))
            .expect("read-only plans dir");
        // Probe: root (and some filesystems) bypass the directory mode.
        let canary = dir.join("permission-probe");
        if std::fs::write(&canary, b"probe").is_ok() {
            let _ = std::fs::remove_file(&canary);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
                .expect("restore plans dir");
            return;
        }
        let result = write_file(&path, "v3 longer");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore plans dir");
        assert!(
            result.is_err(),
            "a read-only plan dir must fail the revision write"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "v2");
        let temps: Vec<String> = std::fs::read_dir(&dir)
            .expect("read plans")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "stale temp files: {temps:?}");
    }

    /// The rename-failure branch cleans its temp file up in the plan
    /// directory (the blocked target lives next to the plan, not in the
    /// temp root).
    #[test]
    fn failed_rename_cleans_up_in_the_plan_dir() {
        let temp = TempDir::new("rename-failure");
        let dir = temp.0.join("plans");
        let path = dir.join("s1-1.md");
        write_file(&path, "v1").expect("first");
        write_file(&path, "v2").expect("second");
        // Renaming the temp file onto an existing directory fails after
        // the temp write succeeded; the previous plan must survive and the
        // temp must be removed from `plans/`.
        let blocked = dir.join("s1-2.md");
        std::fs::create_dir_all(&blocked).expect("blocked dir");
        assert!(write_file(&blocked, "v3").is_err());
        assert!(blocked.is_dir(), "the blocking target is untouched");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "v2");
        let temps: Vec<String> = std::fs::read_dir(&dir)
            .expect("read plans")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "stale temp files: {temps:?}");
    }
}
