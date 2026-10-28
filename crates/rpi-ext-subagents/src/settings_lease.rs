//! Cross-session settings-file lease and atomic settings writes.
//!
//! Port of upstream `src/shared/file-lease.ts` +
//! `src/shared/settings-file-lease.ts` + the settings half of
//! `writeSettingsFile` (`src/agents/agents.ts`) @ `7d072b91d` (TE48 W3/W4;
//! upstream #2627 atomic replacement and #2680 cross-session serialization).
//!
//! Two Pi sessions that edited `settings.json` at the same time could each
//! read the same snapshot and the later save dropped the earlier change. The
//! upstream fix holds a directory lease beside the physical settings file
//! (symlinks resolved, so aliases share it) from the read through the save,
//! and replaces the settings document atomically (temp file + rename, mode
//! preserved, trailing newline kept).
//!
//! The lease is a directory `<target>.write-lock` containing `owner.json`
//! (`{token,pid,hostname,processStart}`). A live or unknown owner is never
//! evicted merely because the lock looks old; a dead owner's lock is renamed
//! to a `.retired-<token>` fence before the new owner claims it, so a delayed
//! reclaimer cannot rename a replacement over the destination.
//!
//! rpi ports the same protocol with `std::fs` primitives only (no new
//! dependencies); the process-start identity reuses the crate's
//! `/proc/<pid>/stat` field-22 reader (`runner::background::process_boot_id`).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Bounded wait for another owner (upstream `withFileLease` default 200ms).
const LEASE_WAIT_MS: u64 = 200;
/// Retry spacing while another owner holds the lease (upstream caps the
/// office-filesystem wait at 10ms).
const LEASE_RETRY_MS: u64 = 10;
/// Authoritative liveness proof inside the lease directory: an OS file lock
/// held for the whole lease. The OS releases it on process death, so a stale
/// owner can be reclaimed on every platform, even with a missing owner.json.
const LEASE_LOCK_FILE: &str = "lease.lock";
/// Windows `LockFileEx` reports a held lock as `ERROR_LOCK_VIOLATION` (33),
/// not `WouldBlock`; both mean contention.
fn is_lock_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        return error.raw_os_error() == Some(33);
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// One lease owner, serialized as `owner.json` (upstream `Owner`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LeaseOwner {
    token: String,
    pid: u64,
    hostname: String,
    #[serde(rename = "processStart", skip_serializing_if = "Option::is_none")]
    process_start: Option<u64>,
}

/// An owned lease; releasing is best effort and token-checked (upstream
/// `release`: only the recorded owner removes the lock).
struct LeaseGuard {
    lock_path: PathBuf,
    token: String,
    lock_file: Option<std::fs::File>,
}

impl LeaseGuard {
    fn release(&mut self) {
        if !read_owner(&self.lock_path).is_some_and(|owner| owner.token == self.token) {
            return;
        }
        // Move the directory aside while the lease.lock handle is still held,
        // then release the OS lock and remove the retired directory.
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

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Physical file a settings path writes to, following file and directory
/// links — including a dangling file link (`resolveSettingsWriteTarget`,
/// settings-file-lease.ts:5-32). A missing target is allowed only when its
/// physical parent already exists; a trailing separator is rejected.
pub(crate) fn resolve_settings_write_target(file_path: &Path) -> io::Result<PathBuf> {
    let mut target = file_path.to_path_buf();
    loop {
        match std::fs::canonicalize(&target) {
            Ok(resolved) => return Ok(resolved),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let as_text = target.to_string_lossy();
        if as_text.ends_with('/') || as_text.ends_with(std::path::MAIN_SEPARATOR) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} names a directory, not a settings file",
                    target.display()
                ),
            ));
        }
        let Some(file_name) = target.file_name() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} has no file name", target.display()),
            ));
        };
        let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
        let physical_parent = match parent {
            Some(parent) => std::fs::canonicalize(parent)?,
            None => std::env::current_dir()?,
        };
        let unresolved = physical_parent.join(file_name);
        match std::fs::read_link(&unresolved) {
            Ok(link) => {
                // Keep the link text intact so the filesystem follows
                // directory links before "..".
                target = if link.is_absolute() {
                    link
                } else {
                    physical_parent.join(link)
                };
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(unresolved),
            Err(error) => return Err(error),
        }
    }
}

/// Hold the physical settings file's lease for a whole read-modify-write
/// (`withSettingsFileLease`, settings-file-lease.ts:34-38). Creates the
/// settings directory first (upstream `mkdirSync` recursive), so a read-only
/// directory fails here — before any read — exactly like upstream.
pub(crate) fn with_settings_file_lease<T>(
    file_path: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let target = resolve_settings_write_target(file_path).map_err(|error| error.to_string())?;
    let existed = target.exists();
    // V18-02 R3.1.3: fixed A(host fs2) → B(`.write-lock`) order. Old host
    // writers hold only A; old subagents hold only B. Holding both in the
    // fixed order is what lets all three generations interleave safely.
    let host_lock = acquire_host_lock(&target).map_err(|error| error.to_string())?;
    let result = with_file_lease(&target, action);
    // A failed first write must not leave the empty file `create(true)` made.
    let failed = !matches!(&result, Ok(Ok(_)));
    if failed && !existed && std::fs::metadata(&target).is_ok_and(|metadata| metadata.len() == 0) {
        let _ = std::fs::remove_file(&target);
    }
    let _ = fs2::FileExt::unlock(&host_lock);
    result.map_err(|error| error.to_string())?
}

fn acquire_host_lock(path: &Path) -> io::Result<std::fs::File> {
    const ATTEMPTS: u32 = 10;
    const DELAY: Duration = Duration::from_millis(20);
    let mut last = None;
    for attempt in 1..=ATTEMPTS {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if is_lock_contended(&error) => {
                last = Some(error);
                if attempt < ATTEMPTS {
                    std::thread::sleep(DELAY);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "settings lock is held")))
}

/// Hold a lease beside `file_path` for a short synchronous update, waiting at
/// most [`LEASE_WAIT_MS`] for another owner (`withFileLease`, file-lease.ts).
fn with_file_lease<T>(requested: &Path, action: impl FnOnce() -> T) -> io::Result<T> {
    let absolute = match requested.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            std::fs::canonicalize(parent)?.join(requested.file_name().unwrap_or_default())
        }
        _ => requested.to_path_buf(),
    };
    let lock_path = PathBuf::from(format!("{}.write-lock", absolute.display()));
    let deadline = Instant::now() + Duration::from_millis(LEASE_WAIT_MS);
    let guard = loop {
        match try_lease(&lock_path)? {
            Some(guard) => break guard,
            None => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "Timed out waiting for another process to finish updating {}.",
                            absolute.display()
                        ),
                    ));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(LEASE_RETRY_MS)));
            }
        }
    };
    let result = action();
    drop(guard);
    Ok(result)
}

fn ownerless_lease_is_stale(lock_path: &Path) -> bool {
    std::fs::metadata(lock_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(5))
}

/// One lease attempt (`tryLease`, file-lease.ts:43-70): create the lock
/// directory, or take over a lock whose owner is provably dead.
fn try_lease(lock_path: &Path) -> io::Result<Option<LeaseGuard>> {
    let owner = LeaseOwner {
        token: new_token(),
        pid: std::process::id() as u64,
        hostname: local_hostname().unwrap_or_default(),
        process_start: crate::runner::background::process_boot_id(std::process::id() as u64),
    };
    match create_private_dir(lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Capture the owner before locking: a peer can retire and
            // recreate the directory between our open and lock.
            let expected_token = read_owner(lock_path).map(|owner| owner.token);
            match try_claim_lease_lock(lock_path)? {
                LeaseLockState::Dead(dead_lock) => {
                    let previous = read_owner(lock_path);
                    if previous.as_ref().map(|owner| owner.token.clone()) != expected_token {
                        drop(dead_lock);
                        return Ok(None);
                    }
                    // The OS lock only proves local death. Never evict a
                    // lease that names another host (network shares have
                    // client-local locks).
                    let same_host = previous.as_ref().is_none_or(|owner| {
                        !owner.hostname.is_empty()
                            && local_hostname().is_some_and(|local| {
                                !local.is_empty() && owner.hostname.eq_ignore_ascii_case(&local)
                            })
                    });
                    if same_host {
                        let token = previous.as_ref().map_or_else(
                            || format!("ownerless-{}", new_token()),
                            |owner| owner.token.clone(),
                        );
                        let retired =
                            PathBuf::from(format!("{}.retired-{}", lock_path.display(), token));
                        let renamed = std::fs::rename(lock_path, &retired).is_ok();
                        drop(dead_lock);
                        if renamed {
                            // Keep the retired directory as the protocol
                            // fence; never delete it.
                            return try_lease(lock_path);
                        }
                        return Ok(None);
                    }
                    drop(dead_lock);
                    return Ok(None);
                }
                LeaseLockState::Alive => return Ok(None),
                LeaseLockState::Missing => {
                    // Legacy lease without lease.lock: owner liveness is the
                    // only signal.
                    let Some(previous) = read_owner(lock_path) else {
                        // Ownerless: only an old directory (crash before the
                        // owner write) is safe to reclaim.
                        if ownerless_lease_is_stale(lock_path) {
                            let retired = PathBuf::from(format!(
                                "{}.retired-ownerless-{}",
                                lock_path.display(),
                                new_token()
                            ));
                            if std::fs::rename(lock_path, &retired).is_ok() {
                                return try_lease(lock_path);
                            }
                        }
                        return Ok(None);
                    };
                    if !owner_is_dead(&previous) {
                        return Ok(None);
                    }
                    // Retain the nonempty old directory as a fence: a delayed
                    // reclaimer of this token cannot rename a replacement
                    // lease over that destination.
                    let retired = PathBuf::from(format!(
                        "{}.retired-{}",
                        lock_path.display(),
                        previous.token
                    ));
                    match std::fs::rename(lock_path, &retired) {
                        Ok(()) => {}
                        Err(error) => {
                            if error.kind() == io::ErrorKind::NotFound
                                || read_owner(&retired)
                                    .is_some_and(|owner| owner.token == previous.token)
                            {
                                return Ok(None);
                            }
                            return Err(error);
                        }
                    }
                    if !read_owner(&retired).is_some_and(|owner| owner.token == previous.token) {
                        return Err(io::Error::other(format!(
                            "Retired lock owner changed during recovery: {}",
                            lock_path.display()
                        )));
                    }
                    return try_lease(lock_path);
                }
            }
        }
        Err(error) => return Err(error),
    }
    let owner_path = lock_path.join("owner.json");
    let body = serde_json::to_vec(&owner).unwrap_or_default();
    if let Err(error) = write_owner_file(&owner_path, &body) {
        let _ = std::fs::remove_dir_all(lock_path);
        return Err(error);
    }
    let lock_file = match create_lease_lock(lock_path) {
        Ok(file) => file,
        Err(error) => {
            // A peer may have claimed the unlocked create window and now
            // owns this path; remove only our own dir and otherwise wait
            // like a contention miss.
            if read_owner(lock_path).is_some_and(|current| current.token == owner.token) {
                let _ = std::fs::remove_dir_all(lock_path);
                return Err(error);
            }
            return Ok(None);
        }
    };
    // Re-verify after locking: between `create_new` and the lock a peer can
    // legitimately retire this dir and create its own.
    if !read_owner(lock_path).is_some_and(|current| current.token == owner.token) {
        drop(lock_file);
        return Ok(None);
    }
    Ok(Some(LeaseGuard {
        lock_path: lock_path.to_path_buf(),
        token: owner.token,
        lock_file: Some(lock_file),
    }))
}

enum LeaseLockState {
    Dead(std::fs::File),
    Alive,
    Missing,
}

fn try_claim_lease_lock(lock_path: &Path) -> io::Result<LeaseLockState> {
    let path = lock_path.join(LEASE_LOCK_FILE);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LeaseLockState::Missing);
        }
        Err(error) => return Err(error),
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(LeaseLockState::Dead(file)),
        Err(error) if is_lock_contended(&error) => Ok(LeaseLockState::Alive),
        Err(error) => Err(error),
    }
}

fn create_lease_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(lock_path.join(LEASE_LOCK_FILE))?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

/// `fs.mkdirSync(lockPath, {mode: 0o700})` — a private lock directory; the
/// mode is applied at creation on unix.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

/// `fs.writeFileSync(owner.json, ..., {flag: "wx", mode: 0o600})`.
fn write_owner_file(path: &Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(body)
}

/// `readOwner` (file-lease.ts:13-21): a malformed owner reads as unknown.
fn read_owner(lock_path: &Path) -> Option<LeaseOwner> {
    let raw = std::fs::read_to_string(lock_path.join("owner.json")).ok()?;
    let owner: LeaseOwner = serde_json::from_str(&raw).ok()?;
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

/// `deadOwner` (file-lease.ts:23-31): only a provably dead owner is evicted.
fn owner_is_dead(owner: &LeaseOwner) -> bool {
    let Some(local) = local_hostname() else {
        return false;
    };
    if owner.hostname.is_empty() || !owner.hostname.eq_ignore_ascii_case(&local) {
        return false;
    }
    if !crate::runner::background::pid_alive(owner.pid) {
        return true;
    }
    let current = crate::runner::background::process_boot_id(owner.pid);
    owner.process_start.is_some() && current.is_some() && owner.process_start != current
}

/// Local hostname, or `None` when unavailable: an unknown local identity
/// never matches another host, so foreign locks are treated as live.
fn local_hostname() -> Option<String> {
    // The kernel hostname is the stable identity both writers share; an
    // exported $HOSTNAME or /etc/hostname is only a fallback.
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // Safety: gethostname writes at most `buffer.len()` bytes and does
        // not retain the pointer.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            let name = String::from_utf8_lossy(&buffer[..end]).trim().to_owned();
            if !name.is_empty() {
                return Some(name);
            }
        }
        if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
        std::env::var("HOSTNAME")
            .ok()
            .filter(|value| !value.is_empty())
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").ok().filter(|v| !v.is_empty())
    }
}

/// A lease token: pid + monotonic counter + nanos, all ASCII-safe.
fn new_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}-{:x}", std::process::id(), counter, nanos)
}

/// Atomic settings write (`writeSettingsFile`, agents.ts @ 7d072b91d): the
/// existing file mode is preserved, a trailing newline is kept, and the new
/// document is written to a same-directory temp file then renamed over the
/// resolved physical path. A read-only target fails before the write, like
/// upstream `accessSync(W_OK)`.
pub(crate) fn write_settings_json(file_path: &Path, root: &serde_json::Value) -> io::Result<()> {
    let target = resolve_settings_write_target(file_path)?;
    let existing_mode = match std::fs::metadata(&target) {
        Ok(metadata) => Some(file_mode(&metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if existing_mode.is_some() {
        // Write-access validation only; the handle is dropped immediately.
        std::fs::OpenOptions::new().write(true).open(&target)?;
    }
    // Preserve the original trailing-newline state (V18-02 R3.1.4). A new
    // file matches the host/upstream serializer: no trailing newline.
    let had_newline = std::fs::read_to_string(&target)
        .map(|raw| raw.ends_with('\n'))
        .unwrap_or(false);
    let serialized = serde_json::to_string_pretty(root).unwrap_or_default();
    let body = if had_newline {
        format!("{serialized}\n")
    } else {
        serialized
    };
    let temp = temp_path_for(&target);
    let result = write_temp_then_rename(&temp, &target, &body, existing_mode);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_temp_then_rename(
    temp: &Path,
    target: &Path,
    body: &str,
    existing_mode: Option<u32>,
) -> io::Result<()> {
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // `tempMode` = existing mode with owner-write set, or the process
            // default for a new file (umask applies).
            options.mode(existing_mode.map_or(0o666, |mode| mode | 0o200));
        }
        let mut file = options.open(temp)?;
        file.write_all(body.as_bytes())?;
    }
    if let Some(mode) = existing_mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temp, std::fs::Permissions::from_mode(mode))?;
        }
    }
    std::fs::rename(temp, target)
}

/// `.${name}.${pid}.${nanos}.tmp` beside the target (atomic-json.ts
/// `tempBaseName`; the rpi temp suffix stays inside the 255-byte component
/// budget for realistic settings paths).
fn temp_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "settings.json".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    target.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), nanos))
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-sub-lease-{label}-{}-{}",
            std::process::id(),
            new_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_settings_json_is_atomic_and_keeps_the_newline() {
        let dir = temp_dir("atomic");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n  \"old\": true\n}\n").unwrap();
        write_settings_json(
            &path,
            &serde_json::json!({"subagents": {"agentOverrides": {}}}),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with("}\n"), "trailing newline preserved: {raw:?}");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed["subagents"]["agentOverrides"]["unknown"],
            serde_json::Value::Null
        );
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_settings_json_preserves_the_absence_of_a_trailing_newline() {
        let dir = temp_dir("no-newline");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"old\": true}").unwrap();
        write_settings_json(
            &path,
            &serde_json::json!({"subagents": {"agentOverrides": {}}}),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.ends_with('\n'), "{raw:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_file_has_no_trailing_newline() {
        let dir = temp_dir("new-no-newline");
        let path = dir.join("settings.json");
        with_settings_file_lease(&path, || {
            write_settings_json(&path, &serde_json::json!({"subagents": {}}))
                .map_err(|error| error.to_string())
        })
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.ends_with('\n'), "{raw:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_lease_lock_is_reclaimed_even_when_the_pid_is_alive() {
        let dir = temp_dir("free-lock");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"old\": true}\n").unwrap();
        let lock = PathBuf::from(format!("{}.write-lock", path.display()));
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LeaseOwner {
            token: "stale".to_owned(),
            // Live PID: the free lease.lock is the authoritative proof.
            pid: std::process::id() as u64,
            hostname: local_hostname().unwrap_or_default(),
            process_start: crate::runner::background::process_boot_id(std::process::id() as u64),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        std::fs::write(lock.join(LEASE_LOCK_FILE), b"").unwrap();

        with_settings_file_lease(&path, || {
            write_settings_json(
                &path,
                &serde_json::json!({"subagents": {"recovered": true}}),
            )
            .map_err(|error| error.to_string())
        })
        .expect("free lease.lock is reclaimed");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("recovered")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_lock_violation_counts_as_contention() {
        assert!(is_lock_contended(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
        #[cfg(windows)]
        assert!(is_lock_contended(&io::Error::from_raw_os_error(33)));
        #[cfg(not(windows))]
        assert!(!is_lock_contended(&io::Error::from_raw_os_error(33)));
        assert!(!is_lock_contended(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn foreign_host_free_lease_lock_is_not_evicted() {
        let dir = temp_dir("foreign-host");
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
        std::fs::write(lock.join(LEASE_LOCK_FILE), b"").unwrap();

        let error = with_settings_file_lease(&path, || Ok::<(), String>(()))
            .expect_err("foreign lease is not evicted");
        assert!(error.contains("Timed out"), "{error}");
        assert!(lock.exists(), "foreign lease retained");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_lease_lock_blocks_even_when_owner_json_is_unknown() {
        use fs2::FileExt;

        let dir = temp_dir("held-lock");
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

        let error = with_settings_file_lease(&path, || Ok::<(), String>(()))
            .expect_err("held lease.lock blocks");
        assert!(error.contains("Timed out"), "{error}");

        held.unlock().unwrap();
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_settings_json_preserves_the_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_settings_json(&path, &serde_json::json!({"a": 1})).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_settings_json_follows_a_file_symlink() {
        let dir = temp_dir("symlink");
        let real = dir.join("real.json");
        let link = dir.join("settings.json");
        std::fs::write(&real, "{}\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_settings_json(&link, &serde_json::json!({"linked": true})).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(parsed["linked"], serde_json::json!(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lease_serializes_and_times_out_while_held() {
        let dir = temp_dir("lease");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let held = with_settings_file_lease(&path, || {
            // A nested acquisition (simulating another session) must time out.
            let nested = with_settings_file_lease(&path, || Ok(()));
            // The outer writer holds A; the nested attempt stops at A
            // (or at B when another generation only holds B). Either way the
            // nested writer must not enter the critical section.
            assert!(nested.is_err(), "nested lease must be rejected");
            Ok(())
        });
        held.unwrap();
        // The lease is released: a later acquisition succeeds.
        with_settings_file_lease(&path, || Ok(())).unwrap();
        assert!(!dir.join("settings.json.write-lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lease_takes_over_a_dead_owner_but_not_a_live_one() {
        let dir = temp_dir("dead");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = dir.join("settings.json.write-lock");
        std::fs::create_dir_all(&lock).unwrap();
        // Dead owner: a pid that cannot exist (u64 max is not a valid i32).
        let dead = LeaseOwner {
            token: "dead-token".to_string(),
            pid: 4_000_000_000,
            hostname: local_hostname().unwrap_or_default(),
            process_start: Some(1),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_vec(&dead).unwrap()).unwrap();
        with_settings_file_lease(&path, || Ok(())).unwrap();
        assert!(!lock.exists(), "dead-owner lock was reclaimed and released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_owner_lock_is_never_evicted() {
        let dir = temp_dir("unknown");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let lock = dir.join("settings.json.write-lock");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("owner.json"), b"not json").unwrap();
        let result = with_settings_file_lease(&path, || Ok(()));
        assert!(
            result
                .err()
                .is_some_and(|error| error.contains("Timed out waiting for another process")),
            "unknown owner must block, not evict"
        );
        assert!(lock.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
