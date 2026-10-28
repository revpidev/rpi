//! Cross-writer settings coordination (ADR-0040 / V18-02 R3.1.3–R3.1.4).
//!
//! Every first-party settings writer takes the same two locks in a fixed
//! order before reading and writing:
//!
//! 1. **A — legacy host `fs2` file lock** on the physical settings file. Old
//!    host paths still only take this lock, so new writers must hold it for
//!    compatibility.
//! 2. **B — `<target>.write-lock` sidecar lease** (the subagents/TE48
//!    protocol: a directory with `owner.json`, dead-owner takeover, bounded
//!    wait). This is the authoritative lock between new writers.
//!
//! A → B is never reversed. Inside the locks the file is re-read, the
//! caller mutates only its own subtree, and the replacement is written to a
//! same-directory temp file, flushed, given the original Unix mode, and
//! renamed. The original trailing-newline state is preserved. On Windows the
//! legacy file handle is closed before the replacement (the design's
//! "close all handles"); B stays held.
//!
//! This module is host-side; the subagents writer mirrors the same protocol
//! in `rpi-ext-subagents/src/settings_lease.rs` (A is added there too). The
//! statusline editor (TE52) must call through the host coordinator rather
//! than inventing a third lock.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

use crate::error::RpiError;

/// Shared lease/mode failure channel.
type WriteResult<T> = Result<T, RpiError>;

const LOCK_MAX_ATTEMPTS: u32 = 10;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
const LEASE_WAIT: Duration = Duration::from_millis(200);
const LEASE_RETRY: Duration = Duration::from_millis(10);

/// Physical settings path (following file and directory symlinks, including a
/// dangling file link). Missing targets are allowed when the physical parent
/// exists.
fn resolve_write_target(file_path: &Path) -> std::io::Result<PathBuf> {
    let mut target = file_path.to_path_buf();
    loop {
        match std::fs::canonicalize(&target) {
            Ok(resolved) => return Ok(resolved),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let text = target.to_string_lossy();
        if text.ends_with('/') || text.ends_with(std::path::MAIN_SEPARATOR) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "{} names a directory, not a settings file",
                    target.display()
                ),
            ));
        }
        let Some(file_name) = target.file_name() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} has no file name", target.display()),
            ));
        };
        let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
        let physical_parent = match parent {
            Some(parent) => match std::fs::canonicalize(parent) {
                Ok(parent) => parent,
                // A missing parent is created later, on the write path.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(target);
                }
                Err(error) => return Err(error),
            },
            None => std::env::current_dir()?,
        };
        let unresolved = physical_parent.join(file_name);
        match std::fs::read_link(&unresolved) {
            Ok(link) => {
                target = if link.is_absolute() {
                    link
                } else {
                    physical_parent.join(link)
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(unresolved);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Acquire A on a file we are about to create. `create_new` distinguishes
/// "we created it" from "another writer won the race"; the latter is a
/// conflict rather than an overwrite of unseen content.
fn acquire_host_lock_new(path: &Path) -> WriteResult<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                RpiError::Settings(format!(
                    "Settings file {} appeared while preparing the write; reload and retry.",
                    path.display()
                ))
            } else {
                RpiError::Io(error)
            }
        })?;
    for attempt in 1..=LOCK_MAX_ATTEMPTS {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt == LOCK_MAX_ATTEMPTS {
                    return Err(RpiError::Settings(format!(
                        "Failed to acquire settings lock for {}: {error}",
                        path.display()
                    )));
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(RpiError::Io(error)),
        }
    }
    Err(RpiError::Settings(format!(
        "Failed to acquire settings lock for {}",
        path.display()
    )))
}

fn acquire_host_lock(path: &Path) -> WriteResult<std::fs::File> {
    for attempt in 1..=LOCK_MAX_ATTEMPTS {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(RpiError::Io)?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt == LOCK_MAX_ATTEMPTS {
                    return Err(RpiError::Settings(format!(
                        "Failed to acquire settings lock for {}: {error}",
                        path.display()
                    )));
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(RpiError::Io(error)),
        }
    }
    Err(RpiError::Settings(format!(
        "Failed to acquire settings lock for {}",
        path.display()
    )))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LeaseOwner {
    token: String,
    pid: u64,
    hostname: String,
    #[serde(rename = "processStart", skip_serializing_if = "Option::is_none")]
    process_start: Option<u64>,
}

struct LeaseGuard {
    lock_path: PathBuf,
    token: String,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if read_owner(&self.lock_path).is_some_and(|owner| owner.token == self.token) {
            let _ = std::fs::remove_dir_all(&self.lock_path);
        }
    }
}

fn read_owner(lock_path: &Path) -> Option<LeaseOwner> {
    let raw = std::fs::read_to_string(lock_path.join("owner.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn owner_is_dead(owner: &LeaseOwner) -> bool {
    if owner.pid == 0 {
        return false;
    }
    // Same-host check: a lease from another host is unknown, never evicted.
    let Ok(hostname) = hostname() else {
        return false;
    };
    if !owner.hostname.is_empty() && owner.hostname != hostname {
        return false;
    }
    // `kill(pid, 0)` is not portable; on Unix the subagents reader compares
    // the process-boot id. Here: a PID that does not exist is dead.
    #[cfg(unix)]
    {
        let pid = owner.pid as i32;
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 performs only an existence/permission check.
        let alive = unsafe { libc_kill(pid, 0) } == 0;
        if alive {
            return false;
        }
        // EPERM means the process exists but is not ours.
        std::io::Error::last_os_error().raw_os_error() != Some(1)
    }
    #[cfg(not(unix))]
    {
        // Best effort on Windows: compare the recorded process start.
        match owner.process_start {
            Some(start) => process_start(owner.pid) != Some(start),
            None => false,
        }
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) -> i32 {
    // `libc` is not a direct dependency; declare the tiny libc surface used
    // by the existence probe.
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    unsafe { kill(pid, signal) }
}

fn hostname() -> std::io::Result<String> {
    #[cfg(unix)]
    {
        if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
            let name = name.trim();
            if !name.is_empty() {
                return Ok(name.to_owned());
            }
        }
    }
    Ok(std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default())
}

fn process_start(pid: u64) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        raw.rsplit(')')
            .next()?
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn acquire_lease(target: &Path) -> WriteResult<LeaseGuard> {
    let lock_path = PathBuf::from(format!("{}.write-lock", target.display()));
    let deadline = Instant::now() + LEASE_WAIT;
    loop {
        let owner = LeaseOwner {
            token: rpi_ai::utils::uuid::random_uuid(),
            pid: std::process::id() as u64,
            hostname: hostname().unwrap_or_default(),
            process_start: process_start(std::process::id() as u64),
        };
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                let body = serde_json::to_vec(&owner).unwrap_or_default();
                if let Err(error) = std::fs::write(lock_path.join("owner.json"), body) {
                    let _ = std::fs::remove_dir_all(&lock_path);
                    return Err(RpiError::Io(error));
                }
                return Ok(LeaseGuard {
                    lock_path,
                    token: owner.token,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let previous = read_owner(&lock_path);
                if let Some(previous) = previous
                    && owner_is_dead(&previous)
                {
                    let retired = PathBuf::from(format!(
                        "{}.retired-{}",
                        lock_path.display(),
                        previous.token
                    ));
                    if std::fs::rename(&lock_path, &retired).is_ok() {
                        continue;
                    }
                }
            }
            Err(error) => return Err(RpiError::Io(error)),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RpiError::Settings(format!(
                "Timed out waiting for another process to finish updating {}.",
                target.display()
            )));
        }
        std::thread::sleep(remaining.min(LEASE_RETRY));
    }
}

fn preserve_trailing_newline(current: Option<&str>, next: &str) -> String {
    let had_newline = current.is_some_and(|text| text.ends_with('\n'));
    let mut body = next.to_owned();
    if had_newline {
        if !body.ends_with('\n') {
            body.push('\n');
        }
    } else {
        while body.ends_with('\n') {
            body.pop();
        }
    }
    body
}

fn atomic_replace(target: &Path, body: &str, mode: Option<u32>) -> WriteResult<()> {
    let temp = target.with_file_name(format!(
        ".{}.{}.{}.tmp",
        target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "settings.json".to_owned()),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> std::io::Result<()> {
        {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(mode.unwrap_or(0o666));
            }
            let mut file = options.open(&temp)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(RpiError::Io)
}

/// Run one coordinated read-modify-write of a settings file.
///
/// `f` receives the current bytes (BOM stripped, trailing-newline state
/// preserved for the write) and returns the next JSON text, or `None` for a
/// read-only operation. Holding both locks across `f` is the point: the
/// caller can re-read and mutate only its own subtree without losing another
/// writer's top-level keys.
pub(crate) fn with_settings_write_lock<T>(
    file_path: &Path,
    f: impl FnOnce(Option<&str>) -> WriteResult<(Option<String>, T)>,
) -> WriteResult<T> {
    let target = resolve_write_target(file_path).map_err(RpiError::Io)?;
    let file_existed = target.exists();
    if !file_existed {
        // A missing settings file must not create its directory on a read-only
        // load. Decide first; only an actual write creates the parent and
        // takes A → B.
        let (next, value) = f(None)?;
        let Some(next) = next else {
            return Ok(value);
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(RpiError::Io)?;
        }
        let host_lock = acquire_host_lock_new(&target)?;
        let lease = acquire_lease(&target)?;
        let body = preserve_trailing_newline(None, &next);
        atomic_replace(&target, &body, None)?;
        let _ = host_lock.unlock();
        drop(lease);
        return Ok(value);
    }

    let host_lock = acquire_host_lock(&target)?;
    let lease = acquire_lease(&target)?;
    let raw = match std::fs::read_to_string(&target) {
        Ok(raw) => Some(crate::tools::edit_diff::strip_bom(&raw).1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(RpiError::Io(error)),
    };
    let mode = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&target)
                .ok()
                .map(|metadata| metadata.permissions().mode() & 0o7777)
        }
        #[cfg(not(unix))]
        {
            None
        }
    };
    let (next, value) = f(raw.as_deref())?;
    if let Some(next) = next {
        let body = preserve_trailing_newline(raw.as_deref(), &next);
        // Windows cannot replace an open file with the legacy handle held;
        // close A first and keep the authoritative B lease.
        #[cfg(windows)]
        {
            let _ = host_lock.unlock();
            drop(host_lock);
            atomic_replace(&target, &body, mode)?;
        }
        #[cfg(not(windows))]
        {
            atomic_replace(&target, &body, mode)?;
            let _ = host_lock.unlock();
        }
    }
    drop(lease);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rpi-settings-write-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn write_preserves_other_keys_newline_and_unix_mode() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n  \"a\": 1,\n  \"other\": {\"keep\": true}\n}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let value = with_settings_write_lock(&path, |current| {
            let mut root: serde_json::Value =
                serde_json::from_str(current.expect("current")).unwrap();
            root["statusLine"] = serde_json::json!({"type": "command"});
            Ok((Some(serde_json::to_string_pretty(&root).unwrap()), 7))
        })
        .unwrap();
        assert_eq!(value, 7);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'), "{raw:?}");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["other"]["keep"], true);
        assert_eq!(parsed["statusLine"]["type"], "command");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_write_lock_blocks_until_it_is_released() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LeaseOwner {
            token: "held".to_owned(),
            pid: std::process::id() as u64,
            hostname: hostname().unwrap_or_default(),
            process_start: None,
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        let error = with_settings_write_lock(&path, |current| Ok((current.map(str::to_owned), ())))
            .expect_err("live lease blocks");
        assert!(error.to_string().contains("Timed out"), "{error}");
        std::fs::remove_dir_all(&lock).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_host_lock_blocks_the_coordinator() {
        use fs2::FileExt;

        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let legacy = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        legacy.lock_exclusive().unwrap();
        let error = with_settings_write_lock(&path, |current| Ok((current.map(str::to_owned), ())))
            .expect_err("legacy A lock blocks the new writer");
        assert!(
            error
                .to_string()
                .contains("Failed to acquire settings lock"),
            "{error}"
        );
        legacy.unlock().unwrap();
        with_settings_write_lock(&path, |current| Ok((current.map(str::to_owned), ())))
            .expect("lock released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_call_does_not_create_a_missing_file() {
        let dir = temp_dir();
        let path = dir.join("missing.json");
        with_settings_write_lock(&path, |current| {
            assert!(current.is_none());
            Ok((None, ()))
        })
        .unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn trailing_newline_absence_is_preserved() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"a\":1}").unwrap();
        with_settings_write_lock(&path, |_current| {
            Ok((Some("{\n  \"b\": 2\n}".to_owned()), ()))
        })
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.ends_with('\n'), "{raw:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_replace_leaves_no_temp_file_on_failure() {
        let dir = temp_dir();
        let target = dir.join("missing-parent").join("settings.json");
        // The parent does not exist, so creating the temp file fails.
        let result = atomic_replace(&target, "{}", None);
        assert!(result.is_err());
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
