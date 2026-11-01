//! A+B coordinated settings writes for the `/statusline` editor (TE52 /
//! ADR-0040 R3.1.3–R3.1.4 / 04 §6).
//!
//! Every first-party writer takes the same two locks in the same order:
//!
//! 1. **A** — the legacy host `fs2` exclusive lock on the physical settings
//!    file (10 × 20 ms retry), kept for writers that predate the lease;
//! 2. **B** — the `<target>.write-lock` directory lease (owner.json +
//!    OS-locked `lease.lock`, dead-owner takeover, bounded 200 ms wait), the
//!    authoritative lock between new writers.
//!
//! Inside the locks the file is re-read, the `statusLine` fingerprint is
//! compared against the value recorded when the editor opened, only that
//! subtree is replaced, and the document is written to a same-directory temp
//! file, flushed, given the original Unix mode, and renamed. The original
//! trailing-newline state is preserved. A conflict, lock timeout, parse
//! failure or IO failure leaves the old bytes and the old running config
//! untouched.
//!
//! The host and the subagents writer implement the identical A→B protocol;
//! this module is the statusline participant, not a third lock.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::{Map, Value};

/// Bounded waits (shared with the host coordinator).
const LOCK_ATTEMPTS: u32 = 10;
const LOCK_RETRY: Duration = Duration::from_millis(20);
const LEASE_WAIT: Duration = Duration::from_millis(200);
const LEASE_RETRY: Duration = Duration::from_millis(10);
const LEASE_LOCK_FILE: &str = "lease.lock";

/// Failure classes the editor maps to its state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteErrorKind {
    /// `statusLine` changed between open and save.
    Conflict,
    /// Another writer held A/B past the bounded wait.
    LockTimeout,
    /// The settings file is not a JSON object.
    Parse,
    /// Filesystem/IO failure.
    Io,
}

/// Structured failure; the message is safe to show in the editor footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError {
    pub kind: WriteErrorKind,
    pub message: String,
}

impl WriteError {
    fn new(kind: WriteErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn io(context: &str, error: io::Error) -> Self {
        Self::new(WriteErrorKind::Io, format!("{context}: {error}"))
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Canonical fingerprint of the `statusLine` subtree: the compact JSON of the
/// current value (`null` when absent). Key order follows the file's
/// preserved order, so whitespace-only edits do not conflict.
pub fn fingerprint(status_line: Option<&Value>) -> String {
    serde_json::to_string(&status_line.cloned().unwrap_or(Value::Null))
        .unwrap_or_else(|_| "null".to_owned())
}

/// Read the settings document and the current `statusLine` fingerprint
/// without taking a lock (the editor records this at open; writes replace the
/// file atomically so a reader sees a complete document).
pub fn read_status_line(path: &Path) -> Result<(Map<String, Value>, String), WriteError> {
    let root = read_root(path)?;
    let current = fingerprint(root.get("statusLine"));
    Ok((root, current))
}

/// Replace the `statusLine` subtree if the fingerprint still matches.
pub fn save_status_line(
    path: &Path,
    expected_fingerprint: &str,
    status_line: &Value,
) -> Result<(), WriteError> {
    let target = resolve_write_target(path).map_err(|error| {
        WriteError::io(
            &format!("resolving settings path {}", path.display()),
            error,
        )
    })?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| WriteError::io("creating the settings directory", error))?;
    }
    let existed = target.exists();
    let host_lock = acquire_host_lock(&target)?;
    let lease = acquire_lease(&target)?;
    let result = (|| {
        let raw = match std::fs::read_to_string(&target) {
            Ok(raw) => Some(strip_bom(&raw).to_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(WriteError::io("reading settings", error)),
        };
        let mut root = parse_object(raw.as_deref())?;
        let current = root.get("statusLine");
        if fingerprint(current) != expected_fingerprint {
            return Err(WriteError::new(
                WriteErrorKind::Conflict,
                "statusLine changed on disk; re-open the editor",
            ));
        }
        root.insert("statusLine".into(), status_line.clone());
        let body = preserve_trailing_newline(raw.as_deref(), &to_json(&root));
        let mode = file_mode(&target);
        #[cfg(windows)]
        {
            // Windows cannot replace an open file with the legacy handle
            // held; B remains authoritative.
            let _ = fs2::FileExt::unlock(&host_lock);
            drop(host_lock);
            atomic_replace(&target, &body, mode)?;
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            atomic_replace(&target, &body, mode)?;
            let _ = fs2::FileExt::unlock(&host_lock);
            Ok(())
        }
    })();
    drop(lease);
    // A failed first write must not leave the empty file `create(true)` made.
    if result.is_err()
        && !existed
        && std::fs::metadata(&target).is_ok_and(|metadata| metadata.len() == 0)
    {
        let _ = std::fs::remove_file(&target);
    }
    result
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

fn read_root(path: &Path) -> Result<Map<String, Value>, WriteError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(WriteError::io("reading settings", error)),
    };
    parse_object(Some(strip_bom(&raw)))
}

fn parse_object(raw: Option<&str>) -> Result<Map<String, Value>, WriteError> {
    let Some(raw) = raw else {
        return Ok(Map::new());
    };
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(WriteError::new(
            WriteErrorKind::Parse,
            "settings.json is not a JSON object",
        )),
        Err(error) => Err(WriteError::new(
            WriteErrorKind::Parse,
            format!("settings.json is not valid JSON: {error}"),
        )),
    }
}

fn to_json(root: &Map<String, Value>) -> String {
    serde_json::to_string_pretty(root).unwrap_or_else(|_| "{}".to_owned())
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

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

/// Physical target (file and directory symlinks followed; a dangling file
/// link resolves to its link text). A missing target is allowed when its
/// physical parent exists.
fn resolve_write_target(file_path: &Path) -> io::Result<PathBuf> {
    let mut target = file_path.to_path_buf();
    loop {
        match std::fs::canonicalize(&target) {
            Ok(resolved) => return Ok(resolved),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let text = target.to_string_lossy();
        if text.ends_with('/') || text.ends_with(std::path::MAIN_SEPARATOR) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} names a directory", target.display()),
            ));
        }
        let Some(file_name) = target.file_name() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no file name"));
        };
        let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
        let physical_parent = match parent {
            Some(parent) => match std::fs::canonicalize(parent) {
                Ok(parent) => parent,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(target),
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(unresolved),
            Err(error) => return Err(error),
        }
    }
}

/// Windows `LockFileEx` reports a held lock as `ERROR_LOCK_VIOLATION` (33).
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

fn acquire_host_lock(path: &Path) -> Result<std::fs::File, WriteError> {
    let mut last = None;
    for attempt in 1..=LOCK_ATTEMPTS {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| WriteError::io("opening settings for locking", error))?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if is_lock_contended(&error) => {
                last = Some(error);
                if attempt < LOCK_ATTEMPTS {
                    std::thread::sleep(LOCK_RETRY);
                }
            }
            Err(error) => return Err(WriteError::io("locking settings", error)),
        }
    }
    let detail = last
        .map(|error| error.to_string())
        .unwrap_or_else(|| "lock held".to_owned());
    Err(WriteError::new(
        WriteErrorKind::LockTimeout,
        format!("timed out waiting for the settings file lock: {detail}"),
    ))
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
    lock_file: Option<std::fs::File>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if !read_owner(&self.lock_path).is_some_and(|owner| owner.token == self.token) {
            return;
        }
        let released = PathBuf::from(format!(
            "{}.released-{}",
            self.lock_path.display(),
            self.token
        ));
        let renamed = std::fs::rename(&self.lock_path, &released).is_ok();
        self.lock_file.take();
        if renamed {
            let _ = std::fs::remove_dir_all(&released);
        } else if read_owner(&self.lock_path).is_some_and(|owner| owner.token == self.token) {
            let _ = std::fs::remove_dir_all(&self.lock_path);
        }
    }
}

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

fn acquire_lease(target: &Path) -> Result<LeaseGuard, WriteError> {
    let lock_path = PathBuf::from(format!("{}.write-lock", target.display()));
    let deadline = Instant::now() + LEASE_WAIT;
    loop {
        match try_lease(&lock_path) {
            Ok(Some(guard)) => return Ok(guard),
            Ok(None) => {}
            Err(error) => return Err(WriteError::io("acquiring the settings lease", error)),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WriteError::new(
                WriteErrorKind::LockTimeout,
                format!(
                    "timed out waiting for another process to update {}",
                    target.display()
                ),
            ));
        }
        std::thread::sleep(remaining.min(LEASE_RETRY));
    }
}

fn try_lease(lock_path: &Path) -> io::Result<Option<LeaseGuard>> {
    let owner = LeaseOwner {
        token: new_token(),
        pid: std::process::id() as u64,
        hostname: local_hostname(),
        process_start: process_start(std::process::id() as u64),
    };
    match create_private_dir(lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let expected_token = read_owner(lock_path).map(|previous| previous.token);
            match try_claim_lease_lock(lock_path)? {
                LeaseLockState::Dead(dead_lock) => {
                    let previous = read_owner(lock_path);
                    if previous.as_ref().map(|owner| owner.token.clone()) != expected_token {
                        drop(dead_lock);
                        return Ok(None);
                    }
                    let same_host = previous.as_ref().is_none_or(|owner| {
                        !owner.hostname.is_empty()
                            && !local_hostname().is_empty()
                            && owner.hostname.eq_ignore_ascii_case(&local_hostname())
                    });
                    if !same_host {
                        drop(dead_lock);
                        return Ok(None);
                    }
                    let token = previous.as_ref().map_or_else(
                        || format!("ownerless-{}", new_token()),
                        |owner| owner.token.clone(),
                    );
                    let retired =
                        PathBuf::from(format!("{}.retired-{}", lock_path.display(), token));
                    let renamed = std::fs::rename(lock_path, &retired).is_ok();
                    drop(dead_lock);
                    if renamed {
                        return try_lease(lock_path);
                    }
                    return Ok(None);
                }
                LeaseLockState::Alive => return Ok(None),
                LeaseLockState::Missing => {
                    let Some(previous) = read_owner(lock_path) else {
                        // Ownerless: only an old directory is safe to reclaim.
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
                    let retired = PathBuf::from(format!(
                        "{}.retired-{}",
                        lock_path.display(),
                        previous.token
                    ));
                    match std::fs::rename(lock_path, &retired) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                        Err(error) => return Err(error),
                    }
                    return try_lease(lock_path);
                }
            }
        }
        Err(error) => return Err(error),
    }
    let body = serde_json::to_vec(&owner).unwrap_or_default();
    if let Err(error) = std::fs::write(lock_path.join("owner.json"), body) {
        let _ = std::fs::remove_dir_all(lock_path);
        return Err(error);
    }
    let lock_file = match create_lease_lock(lock_path) {
        Ok(file) => file,
        Err(error) => {
            if read_owner(lock_path).is_some_and(|current| current.token == owner.token) {
                let _ = std::fs::remove_dir_all(lock_path);
                return Err(error);
            }
            return Ok(None);
        }
    };
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
    match file.try_lock_exclusive() {
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
    file.try_lock_exclusive()?;
    Ok(file)
}

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

fn ownerless_lease_is_stale(lock_path: &Path) -> bool {
    std::fs::metadata(lock_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(5))
}

fn owner_is_dead(owner: &LeaseOwner) -> bool {
    let local = local_hostname();
    if local.is_empty() || owner.hostname.is_empty() || !owner.hostname.eq_ignore_ascii_case(&local)
    {
        return false;
    }
    if !pid_alive(owner.pid) {
        return true;
    }
    match (owner.process_start, process_start(owner.pid)) {
        (Some(recorded), Some(current)) => recorded != current,
        _ => false,
    }
}

fn local_hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_owned();
            }
        }
        std::env::var("HOSTNAME").unwrap_or_default()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u64) -> bool {
    // Unknown liveness: the OS lease.lock remains the authoritative proof;
    // a legacy owner is never evicted on this platform.
    true
}

#[cfg(target_os = "linux")]
fn process_start(pid: u64) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    raw.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn process_start(_pid: u64) -> Option<u64> {
    None
}

fn new_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}-{:x}", std::process::id(), counter, nanos)
}

fn atomic_replace(target: &Path, body: &str, mode: Option<u32>) -> Result<(), WriteError> {
    let temp = target.with_file_name(format!(
        ".{}.{}.{}.tmp",
        target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "settings.json".to_owned()),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> io::Result<()> {
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
    result.map_err(|error| WriteError::io("replacing settings", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-statusline-write-{label}-{}-{}",
            std::process::id(),
            new_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn saves_only_the_statusline_subtree_and_keeps_top_level_keys() {
        let dir = temp_dir("merge");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            "{\n  \"defaultModel\": \"glm-5.1\",\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"echo hi\",\n    \"unknownKey\": 7\n  },\n  \"subagents\": {\n    \"maxDepth\": 3\n  }\n}\n",
        )
        .expect("write");
        let (_, fingerprint) = read_status_line(&path).expect("read");
        let new =
            json!({"type": "items", "items": ["model"], "command": "echo hi", "unknownKey": 7});
        save_status_line(&path, &fingerprint, &new).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(raw.ends_with('\n'), "trailing newline preserved: {raw:?}");
        let parsed: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed["defaultModel"], "glm-5.1");
        assert_eq!(parsed["subagents"]["maxDepth"], 3);
        assert_eq!(parsed["statusLine"], new);
        // No temp files/leases left behind.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp") || name.contains("write-lock"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_statusline_is_a_conflict_and_keeps_the_file() {
        let dir = temp_dir("conflict");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"statusLine\":{\"type\":\"items\",\"items\":[]}}").expect("write");
        let (_, fingerprint) = read_status_line(&path).expect("read");
        // Another writer replaces the subtree after the editor opened.
        std::fs::write(
            &path,
            "{\"statusLine\":{\"type\":\"items\",\"items\":[\"model\"]}}",
        )
        .expect("rewrite");
        let before = std::fs::read(&path).expect("bytes");
        let error =
            save_status_line(&path, &fingerprint, &json!({"type": "items"})).expect_err("conflict");
        assert_eq!(error.kind, WriteErrorKind::Conflict);
        assert_eq!(std::fs::read(&path).expect("bytes"), before, "old bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_top_level_keys_written_after_open_survive_the_save() {
        let dir = temp_dir("three-way");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            "{\"defaultModel\":\"old\",\"statusLine\":{\"type\":\"items\",\"items\":[]},\"subagents\":{\"maxDepth\":1}}",
        )
        .expect("write");
        let (_, fingerprint) = read_status_line(&path).expect("read");
        // Host writer updates defaultModel while the editor is open.
        let mut root: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        root["defaultModel"] = json!("new");
        std::fs::write(&path, serde_json::to_string(&root).expect("json")).expect("rewrite");
        // Old-protocol subagents writer adds its own subtree.
        let mut root: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        root["subagents"]["enabled"] = json!(true);
        std::fs::write(&path, serde_json::to_string(&root).expect("json")).expect("rewrite");
        // Statusline saves only its subtree.
        save_status_line(
            &path,
            &fingerprint,
            &json!({"type": "items", "items": ["model"], "placement": "widget"}),
        )
        .expect("save");
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(parsed["defaultModel"], "new");
        assert_eq!(parsed["subagents"]["maxDepth"], 1);
        assert_eq!(parsed["subagents"]["enabled"], true);
        assert_eq!(parsed["statusLine"]["items"], json!(["model"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_lease_times_out_without_touching_the_file() {
        let dir = temp_dir("timeout");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"statusLine\":{\"type\":\"items\"}}").expect("write");
        let (_, fingerprint) = read_status_line(&path).expect("read");
        let before = std::fs::read(&path).expect("bytes");
        // Hold B from another thread (a different "process" for our purposes:
        // the lock directory + locked lease.lock).
        let lock_path = PathBuf::from(format!("{}.write-lock", path.display()));
        let holder = std::thread::spawn(move || {
            std::fs::create_dir_all(&lock_path).expect("mkdir");
            std::fs::write(
                lock_path.join("owner.json"),
                serde_json::to_vec(&LeaseOwner {
                    token: "held".into(),
                    pid: std::process::id() as u64,
                    hostname: local_hostname(),
                    process_start: process_start(std::process::id() as u64),
                })
                .expect("owner"),
            )
            .expect("write owner");
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(lock_path.join(LEASE_LOCK_FILE))
                .expect("lease lock");
            file.lock_exclusive().expect("lock");
            std::thread::sleep(Duration::from_millis(500));
            let _ = fs2::FileExt::unlock(&file);
            let _ = std::fs::remove_dir_all(&lock_path);
        });
        std::thread::sleep(Duration::from_millis(50));
        let error =
            save_status_line(&path, &fingerprint, &json!({"type": "items"})).expect_err("timeout");
        assert_eq!(error.kind, WriteErrorKind::LockTimeout);
        assert_eq!(std::fs::read(&path).expect("bytes"), before, "old bytes");
        holder.join().expect("holder");
        // After release the save succeeds.
        save_status_line(&path, &fingerprint, &json!({"type": "items"}))
            .expect("save after release");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_json_is_a_parse_error_and_is_not_overwritten() {
        let dir = temp_dir("parse");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ not json").expect("write");
        let error = read_status_line(&path).expect_err("parse");
        assert_eq!(error.kind, WriteErrorKind::Parse);
        let error = save_status_line(&path, "null", &json!({"type": "items"})).expect_err("parse");
        assert_eq!(error.kind, WriteErrorKind::Parse);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bom_is_stripped_and_a_new_file_has_no_trailing_newline() {
        let dir = temp_dir("bom-new");
        let path = dir.join("settings.json");
        std::fs::write(&path, "\u{feff}{\"a\":1}").expect("write");
        let (_, fingerprint) = read_status_line(&path).expect("read");
        save_status_line(&path, &fingerprint, &json!({"type": "items"})).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(!raw.starts_with('\u{feff}'));
        assert!(!raw.ends_with('\n'));
        // Missing file: a fresh document is created and valid.
        let fresh = dir.join("fresh.json");
        let (_, fingerprint) = read_status_line(&fresh).expect("read missing");
        assert_eq!(fingerprint, "null");
        save_status_line(&fresh, "null", &json!({"type": "items"})).expect("save fresh");
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&fresh).expect("read")).expect("json");
        assert_eq!(parsed["statusLine"]["type"], "items");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_existing_unix_mode_is_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"statusLine\":{\"type\":\"items\"}}").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let (_, fingerprint) = read_status_line(&path).expect("read");
        save_status_line(&path, &fingerprint, &json!({"type": "items"})).expect("save");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprints_ignore_key_order_only_when_the_value_is_identical() {
        let first: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).expect("json");
        let second: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).expect("json");
        // preserve_order keeps the file order: a reordered object is a
        // different fingerprint, which is a conservative conflict.
        assert_ne!(fingerprint(Some(&first)), fingerprint(Some(&second)));
        assert_eq!(fingerprint(None), "null");
    }
}
