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
const SETTINGS_FILE_APPEARED: &str = "appeared while preparing the write";
/// Windows `fs2`/`LockFileEx` reports a held lock as
/// `ERROR_LOCK_VIOLATION` (33), not `WouldBlock`; both mean contention.
fn is_lock_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // `LockFileEx` reports a held lock as ERROR_LOCK_VIOLATION (33).
        return error.raw_os_error() == Some(33);
    }
    #[cfg(not(windows))]
    {
        false
    }
}

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
                    "Settings file {} {SETTINGS_FILE_APPEARED}; reload and retry.",
                    path.display()
                ))
            } else {
                RpiError::Io(error)
            }
        })?;
    for attempt in 1..=LOCK_MAX_ATTEMPTS {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if is_lock_contended(&error) => {
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
            Err(error) if is_lock_contended(&error) => {
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

const LEASE_LOCK_FILE: &str = "lease.lock";

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
    /// Authoritative liveness proof: an OS file lock held for the whole lease.
    /// The OS releases it when the process dies, so a dead owner can always be
    /// reclaimed on every supported platform (including Windows), even when
    /// `owner.json` is missing or truncated.
    lock_file: Option<std::fs::File>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if !read_owner(&self.lock_path).is_some_and(|owner| owner.token == self.token) {
            return;
        }
        // Move the owned directory aside while the lease.lock handle is still
        // held, then release the OS lock and remove the released directory. A
        // concurrent reclaimer never observes a half-removed live lease.
        let released = PathBuf::from(format!(
            "{}.released-{}",
            self.lock_path.display(),
            self.token
        ));
        let renamed = std::fs::rename(&self.lock_path, &released).is_ok();
        self.lock_file.take();
        if renamed {
            // The released path is ours alone; no new owner can appear there.
            let _ = std::fs::remove_dir_all(&released);
        } else if read_owner(&self.lock_path).is_some_and(|owner| owner.token == self.token) {
            // Rename failed (e.g. a Windows handle); only remove the original
            // path while it still carries our token.
            let _ = std::fs::remove_dir_all(&self.lock_path);
        }
    }
}

fn read_owner(lock_path: &Path) -> Option<LeaseOwner> {
    let raw = std::fs::read_to_string(lock_path.join("owner.json")).ok()?;
    let owner: LeaseOwner = serde_json::from_str(&raw).ok()?;
    // Match the subagents reader: a malformed owner is unknown, never a
    // reason to evict a live lease.
    let token_ok = !owner.token.is_empty()
        && owner.token.len() <= 64
        && owner
            .token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !token_ok || owner.pid == 0 {
        return None;
    }
    Some(owner)
}

fn owner_is_dead(owner: &LeaseOwner) -> bool {
    if owner.pid == 0 {
        return false;
    }
    // Same-host check: a lease from another host is unknown, never evicted.
    let Ok(hostname) = hostname() else {
        return false;
    };
    // An empty local/recorded hostname is unknown, never a licence to evict.
    if hostname.is_empty()
        || owner.hostname.is_empty()
        || !owner.hostname.eq_ignore_ascii_case(&hostname)
    {
        return false;
    }
    // Legacy leases (no `lease.lock`) fall back to process liveness. New
    // leases are reclaimed by the OS lock alone; this path only protects
    // writers from older versions.
    #[cfg(unix)]
    {
        let pid = owner.pid as i32;
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 performs only an existence/permission check.
        let alive = unsafe { libc_kill(pid, 0) } == 0;
        if alive {
            // A live PID whose recorded start time differs is a reused PID:
            // the original owner is dead.
            if let (Some(recorded), Some(current)) = (owner.process_start, process_start(owner.pid))
            {
                return recorded != current;
            }
            return false;
        }
        // EPERM means the process exists but is not ours.
        std::io::Error::last_os_error().raw_os_error() != Some(1)
    }
    #[cfg(not(unix))]
    {
        // Best effort on Windows for legacy leases: compare the recorded
        // process start. New leases use the lease.lock file lock instead.
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

#[cfg(unix)]
unsafe fn gethostname(buffer: *mut u8, len: usize) -> i32 {
    unsafe extern "C" {
        fn gethostname(name: *mut std::ffi::c_char, len: usize) -> i32;
    }
    unsafe { gethostname(buffer.cast(), len) }
}

fn hostname() -> std::io::Result<String> {
    // The kernel hostname is the stable identity both writers share; an
    // exported $HOSTNAME or /etc/hostname is only a fallback. On Windows the
    // extension writer uses COMPUTERNAME only, so the host must do the same.
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        if unsafe { gethostname(buffer.as_mut_ptr(), buffer.len()) } == 0 {
            let end = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            let name = String::from_utf8_lossy(&buffer[..end]).trim().to_owned();
            if !name.is_empty() {
                return Ok(name);
            }
        }
        if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
            let name = name.trim();
            if !name.is_empty() {
                return Ok(name.to_owned());
            }
        }
        Ok(std::env::var("HOSTNAME").unwrap_or_default())
    }
    #[cfg(not(unix))]
    {
        Ok(std::env::var("COMPUTERNAME").unwrap_or_default())
    }
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

enum LeaseLockState {
    /// We acquired the previous owner's lock: the owner is dead.
    Dead(std::fs::File),
    /// The lock file exists and is held by a live owner.
    Alive,
    /// A legacy lease without `lease.lock`; use owner liveness.
    Missing,
}

fn try_claim_lease_lock(lock_path: &Path) -> WriteResult<LeaseLockState> {
    let path = lock_path.join(LEASE_LOCK_FILE);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LeaseLockState::Missing);
        }
        Err(error) => return Err(RpiError::Io(error)),
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(LeaseLockState::Dead(file)),
        Err(error) if is_lock_contended(&error) => Ok(LeaseLockState::Alive),
        Err(error) => Err(RpiError::Io(error)),
    }
}

fn create_lease_lock(lock_path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(lock_path.join(LEASE_LOCK_FILE))?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn retired_lease_path(lock_path: &Path, token: &str) -> PathBuf {
    PathBuf::from(format!("{}.retired-{}", lock_path.display(), token))
}

fn ownerless_lease_is_stale(lock_path: &Path) -> bool {
    std::fs::metadata(lock_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(5))
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
                // Write owner.json before creating lease.lock: a peer that
                // sees a lease directory must never observe a lock file
                // without owner metadata (that window would look dead).
                let body = serde_json::to_vec(&owner).unwrap_or_default();
                if let Err(error) = std::fs::write(lock_path.join("owner.json"), body) {
                    let _ = std::fs::remove_dir_all(&lock_path);
                    return Err(RpiError::Io(error));
                }
                let lock_file = match create_lease_lock(&lock_path) {
                    Ok(file) => file,
                    Err(error) => {
                        // A peer may have claimed the unlocked create window
                        // and now owns this path; remove only our own dir and
                        // otherwise wait/retry like a contention miss.
                        if read_owner(&lock_path)
                            .is_some_and(|current| current.token == owner.token)
                        {
                            let _ = std::fs::remove_dir_all(&lock_path);
                            return Err(RpiError::Io(error));
                        }
                        std::thread::sleep(LEASE_RETRY);
                        continue;
                    }
                };
                // Re-verify after locking: between `create_new` and
                // `try_lock_exclusive` the file exists unlocked, so a peer
                // can legitimately classify us as dead and retire the dir.
                if !read_owner(&lock_path).is_some_and(|current| current.token == owner.token) {
                    drop(lock_file);
                    std::thread::sleep(LEASE_RETRY);
                    continue;
                }
                return Ok(LeaseGuard {
                    lock_path,
                    token: owner.token,
                    lock_file: Some(lock_file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Capture the owner before locking: a peer can retire and
                // recreate the directory between our open and lock.
                let expected_token = read_owner(&lock_path).map(|owner| owner.token);
                match try_claim_lease_lock(&lock_path)? {
                    LeaseLockState::Dead(dead_lock) => {
                        let previous = read_owner(&lock_path);
                        if previous.as_ref().map(|owner| owner.token.clone()) != expected_token {
                            drop(dead_lock);
                            std::thread::sleep(LEASE_RETRY);
                            continue;
                        }
                        // The OS lock only proves local death. A lease that
                        // names another host is not ours to evict (network
                        // shares have client-local locks).
                        let same_host = previous.as_ref().is_none_or(|owner| {
                            hostname().is_ok_and(|local| {
                                !local.is_empty()
                                    && !owner.hostname.is_empty()
                                    && owner.hostname.eq_ignore_ascii_case(&local)
                            })
                        });
                        if same_host {
                            let token = previous.as_ref().map_or_else(
                                || format!("ownerless-{}", rpi_ai::utils::uuid::random_uuid()),
                                |owner| owner.token.clone(),
                            );
                            let retired = retired_lease_path(&lock_path, &token);
                            let renamed = std::fs::rename(&lock_path, &retired).is_ok();
                            drop(dead_lock);
                            // Keep the retired directory as the protocol
                            // fence; a delayed reclaimer must not rename over
                            // a replacement lease.
                            if renamed {
                                continue;
                            }
                        } else {
                            drop(dead_lock);
                        }
                    }
                    LeaseLockState::Alive => {}
                    LeaseLockState::Missing => {
                        match read_owner(&lock_path) {
                            // Legacy writer: owner liveness is the only signal.
                            Some(previous) if owner_is_dead(&previous) => {
                                let retired = retired_lease_path(&lock_path, &previous.token);
                                if std::fs::rename(&lock_path, &retired).is_ok() {
                                    continue;
                                }
                            }
                            Some(_) => {}
                            // Crash between mkdir and owner.json: the tiny
                            // window is not safe to evict immediately, but an
                            // old ownerless directory is.
                            None if ownerless_lease_is_stale(&lock_path) => {
                                let retired = retired_lease_path(
                                    &lock_path,
                                    &format!("ownerless-{}", rpi_ai::utils::uuid::random_uuid()),
                                );
                                if std::fs::rename(&lock_path, &retired).is_ok() {
                                    continue;
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
            Err(error) => return Err(RpiError::Io(error)),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RpiError::Settings(format!(
                "Timed out waiting for another process to finish updating {}. If no other rpi process is running, delete {}.write-lock.",
                target.display(),
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
///
/// `f` may be called twice when another process creates a missing settings
/// file between the pre-lock decision and the `create_new` A acquisition; the
/// retry then runs against the existing file with both locks held.
pub(crate) fn with_settings_write_lock<T>(
    file_path: &Path,
    mut f: impl FnMut(Option<&str>) -> WriteResult<(Option<String>, T)>,
) -> WriteResult<T> {
    let mut retried = false;
    loop {
        match with_settings_write_lock_once(file_path, &mut f) {
            Err(RpiError::Settings(message))
                if !retried && message.contains(SETTINGS_FILE_APPEARED) =>
            {
                retried = true;
                continue;
            }
            other => return other,
        }
    }
}

fn with_settings_write_lock_once<T>(
    file_path: &Path,
    f: &mut impl FnMut(Option<&str>) -> WriteResult<(Option<String>, T)>,
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
        #[cfg(windows)]
        {
            let _ = host_lock.unlock();
            drop(host_lock);
            atomic_replace(&target, &body, None)?;
        }
        #[cfg(not(windows))]
        {
            atomic_replace(&target, &body, None)?;
            let _ = host_lock.unlock();
        }
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

/// Read a settings file under the legacy A lock only.
///
/// B (`<target>.write-lock`) is the write lease. A pure read does not need
/// it: writes replace the file with a same-directory rename, so a reader
/// always sees a complete old or new document. Skipping B keeps a stale or
/// crashed writer's lease from making settings unreadable (the 200 ms B wait
/// used to turn a wedged lease into an empty in-memory settings load).
pub(crate) fn with_settings_read_lock<T>(
    file_path: &Path,
    f: impl FnOnce(Option<&str>) -> WriteResult<T>,
) -> WriteResult<T> {
    let target = resolve_write_target(file_path).map_err(RpiError::Io)?;
    let file = match std::fs::OpenOptions::new().read(true).open(&target) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return f(None),
        Err(error) => return Err(RpiError::Io(error)),
    };
    for attempt in 1..=LOCK_MAX_ATTEMPTS {
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => break,
            Err(error) if is_lock_contended(&error) => {
                if attempt == LOCK_MAX_ATTEMPTS {
                    return Err(RpiError::Settings(format!(
                        "Failed to acquire settings lock for {}: {error}",
                        target.display()
                    )));
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(RpiError::Io(error)),
        }
    }
    let raw = match std::fs::read_to_string(&target) {
        Ok(raw) => Some(crate::tools::edit_diff::strip_bom(&raw).1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(RpiError::Io(error)),
    };
    let value = f(raw.as_deref())?;
    let _ = fs2::FileExt::unlock(&file);
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
    fn stale_lease_lock_is_reclaimed_even_when_the_pid_is_alive() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LeaseOwner {
            token: "stale".to_owned(),
            // Deliberately the current (live) PID: the free lease.lock is the
            // authoritative proof that the previous owner is gone.
            pid: std::process::id() as u64,
            hostname: hostname().unwrap_or_default(),
            process_start: None,
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        std::fs::write(lock.join(LEASE_LOCK_FILE), b"").unwrap();

        with_settings_write_lock(&path, |_current| {
            Ok((Some("{\n  \"ok\": true\n}".to_owned()), ()))
        })
        .expect("free lease.lock is reclaimed");

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("ok"), "{raw}");
        assert!(!lock.exists(), "retired lease removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_lock_violation_counts_as_contention() {
        assert!(is_lock_contended(&std::io::Error::from(
            std::io::ErrorKind::WouldBlock
        )));
        #[cfg(windows)]
        assert!(is_lock_contended(&std::io::Error::from_raw_os_error(33)));
        #[cfg(not(windows))]
        assert!(!is_lock_contended(&std::io::Error::from_raw_os_error(33)));
        assert!(!is_lock_contended(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn foreign_host_free_lease_lock_is_not_evicted() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LeaseOwner {
            token: "foreign".to_owned(),
            pid: std::process::id() as u64,
            hostname: "definitely-another-host".to_owned(),
            process_start: None,
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        // A client-local lock can be acquired on a network share even while
        // the owning host is alive; the foreign hostname must still win.
        std::fs::write(lock.join(LEASE_LOCK_FILE), b"").unwrap();

        let error = with_settings_write_lock(&path, |current| Ok((current.map(str::to_owned), ())))
            .expect_err("foreign lease is not evicted");
        assert!(error.to_string().contains("Timed out"), "{error}");
        assert!(lock.exists(), "foreign lease retained");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn old_ownerless_lease_without_lock_file_is_reclaimed() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        // Simulate a crash between mkdir and owner.json: no lock file, no
        // owner, but an old directory.
        let file = std::fs::File::open(&lock).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(10);
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(file);

        with_settings_write_lock(&path, |_current| {
            Ok((Some("{\n  \"reclaimed\": true\n}".to_owned()), ()))
        })
        .expect("stale ownerless lease reclaimed");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("reclaimed")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_lease_lock_blocks_even_when_the_owner_json_is_unknown() {
        use fs2::FileExt;

        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock.join(LEASE_LOCK_FILE))
            .unwrap();
        held.lock_exclusive().unwrap();

        let error = with_settings_write_lock(&path, |current| Ok((current.map(str::to_owned), ())))
            .expect_err("held lease.lock blocks");
        assert!(error.to_string().contains("Timed out"), "{error}");

        held.unlock().unwrap();
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_owner_with_a_free_lease_lock_is_reclaimed() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join(LEASE_LOCK_FILE), b"").unwrap();

        with_settings_write_lock(&path, |_current| {
            Ok((Some("{\n  \"recovered\": true\n}".to_owned()), ()))
        })
        .expect("ownerless lease with a free lock is reclaimed");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("recovered")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ownerless_free_lock_reclaims_use_a_unique_fence() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));

        for attempt in 0..2 {
            std::fs::create_dir_all(&lock).unwrap();
            std::fs::write(lock.join(LEASE_LOCK_FILE), b"").unwrap();
            with_settings_write_lock(&path, |_current| {
                Ok((Some(format!("{{\n  \"attempt\": {attempt}\n}}")), ()))
            })
            .unwrap_or_else(|error| panic!("attempt {attempt}: {error}"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_path_ignores_a_held_write_lease() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n  \"a\": 1\n}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock.join(LEASE_LOCK_FILE))
            .unwrap();
        held.lock_exclusive().unwrap();

        let mut seen = None;
        with_settings_read_lock(&path, |current| {
            seen = current.map(str::to_owned);
            Ok(())
        })
        .expect("read does not wait on B");
        assert!(seen.unwrap().contains("\"a\""));

        held.unlock().unwrap();
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Three writer generations touch one file: the host coordinator
    /// (A+B), a second new writer (A+B) and an old protocol writer that only
    /// holds A. Every writer re-reads under A, so no top-level key is lost.
    #[test]
    fn three_writer_generations_preserve_each_others_keys() {
        use fs2::FileExt;

        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let path_a = path.clone();
        let path_b = path.clone();
        let path_legacy = path.clone();

        let a = std::thread::spawn(move || {
            with_settings_write_lock(&path_a, |current| {
                let mut root: serde_json::Value =
                    serde_json::from_str(current.expect("current")).unwrap();
                root["defaultModel"] = serde_json::json!("model-a");
                Ok((Some(serde_json::to_string_pretty(&root).unwrap()), ()))
            })
            .unwrap();
        });
        let b = std::thread::spawn(move || {
            with_settings_write_lock(&path_b, |current| {
                let mut root: serde_json::Value =
                    serde_json::from_str(current.expect("current")).unwrap();
                root["statusLine"] = serde_json::json!({"type": "command"});
                Ok((Some(serde_json::to_string_pretty(&root).unwrap()), ()))
            })
            .unwrap();
        });
        let legacy = std::thread::spawn(move || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path_legacy)
                .unwrap();
            file.lock_exclusive().unwrap();
            let raw = std::fs::read_to_string(&path_legacy).unwrap();
            let mut root: serde_json::Value = serde_json::from_str(&raw).unwrap();
            root["subagents"] = serde_json::json!({"agentOverrides": {}});
            atomic_replace(
                &path_legacy,
                &format!("{}\n", serde_json::to_string_pretty(&root).unwrap()),
                None,
            )
            .unwrap();
            file.unlock().unwrap();
        });
        a.join().unwrap();
        b.join().unwrap();
        legacy.join().unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["defaultModel"], "model-a");
        assert_eq!(root["statusLine"]["type"], "command");
        assert_eq!(root["subagents"]["agentOverrides"], serde_json::json!({}));
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
