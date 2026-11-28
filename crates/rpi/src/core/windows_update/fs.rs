//! The file-system seam of the update machinery (design §11.12).
//!
//! Every file-system operation of the state machine, journal, rollback,
//! pins, GC and pointer logic goes through [`UpdateFs`]. Production uses
//! [`RealFs`]; tests wrap it in [`SpyFs`], which records every call (so a
//! "no pending: exactly one existence query" budget is an assertion) and
//! injects failures at a chosen call, including transient ones a Windows
//! rename retry has to survive.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// The operation kinds a test can count or fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsOp {
    Exists,
    IsDir,
    Read,
    ReadDir,
    Modified,
    CreateDirAll,
    WriteAtomic,
    Rename,
    RemoveFile,
    RemoveDirAll,
    HashFile,
    ProbeDeletable,
    Append,
    FileLen,
}

impl FsOp {
    pub fn name(self) -> &'static str {
        match self {
            FsOp::Exists => "exists",
            FsOp::IsDir => "is_dir",
            FsOp::Read => "read",
            FsOp::ReadDir => "read_dir",
            FsOp::Modified => "modified",
            FsOp::CreateDirAll => "create_dir_all",
            FsOp::WriteAtomic => "write_atomic",
            FsOp::Rename => "rename",
            FsOp::RemoveFile => "remove_file",
            FsOp::RemoveDirAll => "remove_dir_all",
            FsOp::HashFile => "sha256_file",
            FsOp::ProbeDeletable => "probe_deletable",
            FsOp::Append => "append",
            FsOp::FileLen => "file_len",
        }
    }
}

/// The operations the update code performs. Removals treat a missing path as
/// success (undo steps are idempotent); `rename` replaces an existing file
/// destination and fails when the destination is a non-empty directory.
pub trait UpdateFs: Send + Sync {
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn is_dir(&self, path: &Path) -> io::Result<bool>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsEntry>>;
    fn modified(&self, path: &Path) -> io::Result<SystemTime>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Write `bytes` through a same-directory temporary file and a rename
    /// over `path` (the state files are crash-consistent stores).
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Lowercase hex sha256 of the file at `path`.
    fn sha256_file(&self, path: &Path) -> io::Result<String>;
    /// Fail when `path` cannot be deleted right now (opened elsewhere
    /// without share-delete, or no permission); succeed without changing
    /// anything otherwise.
    fn probe_deletable(&self, path: &Path) -> io::Result<()>;
    /// Append `bytes` to `path` (the log). The default is a read-modify-write
    /// over [`Self::write_atomic`]; the real file system appends in place.
    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut next = self.read(path).unwrap_or_default();
        next.extend_from_slice(bytes);
        self.write_atomic(path, &next)
    }
    /// Size of `path` in bytes; `Ok(0)` when it does not exist.
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(self.read(path).map(|bytes| bytes.len() as u64).unwrap_or(0))
    }
    /// The identity of the image at `path` (length and modification time),
    /// the same marks a pin records for the image its process started from.
    fn exe_identity(&self, path: &Path) -> io::Result<super::records::ExeIdentity> {
        let len = self.file_len(path)?;
        let modified_unix_ms = self
            .modified(path)?
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis();
        Ok(super::records::ExeIdentity {
            len,
            modified_unix_ms: u64::try_from(modified_unix_ms).unwrap_or(u64::MAX),
        })
    }
    /// The wall clock, ISO 8601 UTC (`2026-11-28T01:02:03Z`).
    fn now_iso8601(&self) -> String;
    /// The pause between rename retries (tests record it instead).
    fn retry_sleep(&self, duration: std::time::Duration) {
        std::thread::sleep(duration);
    }
}

/// A missing path is not an error for the removal operations.
pub fn is_not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// `std::fs` with the Windows-specific probes supplied by the platform
/// module.
pub struct RealFs;

impl UpdateFs for RealFs {
    fn exists(&self, path: &Path) -> io::Result<bool> {
        Ok(path.exists())
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        std::fs::metadata(path).map(|metadata| metadata.is_dir())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsEntry>> {
        let mut entries: Vec<FsEntry> = std::fs::read_dir(path)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| FsEntry {
                is_dir: entry.file_type().is_ok_and(|file_type| file_type.is_dir()),
                path: entry.path(),
            })
            .collect();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        std::fs::metadata(path)?.modified()
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        atomic_write_bytes(path, bytes)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if is_not_found(&error) => Ok(()),
            other => other,
        }
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Err(error) if is_not_found(&error) => Ok(()),
            other => other,
        }
    }

    fn sha256_file(&self, path: &Path) -> io::Result<String> {
        sha256_file(path)
    }

    fn probe_deletable(&self, path: &Path) -> io::Result<()> {
        probe_deletable(path)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        file.write_all(bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0))
    }

    fn now_iso8601(&self) -> String {
        crate::core::session_manager::now_iso8601()
    }
}

/// Write `bytes` to `<path>.tmp-<pid>-<nanos>` in the same directory, flush
/// it to disk, then rename it over `path`. The temporary file is removed on
/// failure.
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}-{nanos}", std::process::id()));
    let temp = PathBuf::from(temp);
    let result = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Lowercase hex sha256 of a file, streamed.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex of a digest.
pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// [`UpdateFs::probe_deletable`] for the real file system: on Windows, open
/// `path` with `DELETE` access and every sharing mode, then close the handle
/// — a sharing violation or an access denial surfaces here, before anything
/// is renamed or deleted. Elsewhere the check is the metadata lookup
/// (removal itself may still fail; tests inject the in-use case through
/// `SpyFs`).
pub fn probe_deletable(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        super::platform::open_for_delete(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::symlink_metadata(path).map(|_| ())
    }
}

/// Whether a failed rename is worth retrying: Windows reports a transient
/// sharing violation (32) or an access denial (5) while a scanner holds the
/// file; a test injects the same codes through [`SpyFs`].
pub fn is_transient_rename_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32) | Some(5))
}

/// Backoff between rename retries: 50, 100, 150, 200 ms.
pub fn rename_backoff(attempt: u32) -> std::time::Duration {
    // 50, 100, 150, 200, 200 ms: capped at 200.
    std::time::Duration::from_millis((50 * u64::from(attempt.max(1))).min(200))
}

/// Rename with bounded retries on transient failures (design §11.5). The
/// sleep between attempts is injected so tests run without waiting.
pub fn rename_with_retry(
    fs: &dyn UpdateFs,
    from: &Path,
    to: &Path,
    retries: u32,
    sleep: &dyn Fn(std::time::Duration),
) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        match fs.rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if is_transient_rename_error(&error) && attempt < retries => {
                sleep(rename_backoff(attempt + 1));
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// One recorded call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsCall {
    pub op: FsOp,
    pub path: PathBuf,
}

/// The error an injected failure raises: an OS code with its kind, or the
/// default "injected <op> failure" text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InjectedError {
    Default,
    Os(io::ErrorKind, Option<i32>, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fault {
    op: FsOp,
    /// Matched as a substring of the path (so tests name a file, not a
    /// full temp path).
    path_contains: String,
    remaining: usize,
    error: InjectedError,
}

impl InjectedError {
    fn from(error: &io::Error) -> Self {
        InjectedError::Os(error.kind(), error.raw_os_error(), error.to_string())
    }

    fn make(&self, op: FsOp, path_contains: &str) -> io::Error {
        match self {
            InjectedError::Default => io::Error::other(format!(
                "injected {} failure for a path containing {path_contains:?}",
                op.name()
            )),
            InjectedError::Os(kind, raw, message) => match raw {
                Some(raw) => io::Error::from_raw_os_error(*raw),
                None => io::Error::new(*kind, message.clone()),
            },
        }
    }
}

impl Fault {
    fn make_error(&self) -> io::Error {
        self.error.make(self.op, &self.path_contains)
    }
}

/// [`RealFs`] with a call log and fault injection.
pub struct SpyFs {
    inner: RealFs,
    state: Mutex<SpyState>,
    /// Wall clock override for deterministic `installedAt` comparisons.
    now: Mutex<Option<String>>,
}

#[derive(Default)]
struct SpyState {
    calls: Vec<FsCall>,
    faults: Vec<Fault>,
    sleeps: Vec<std::time::Duration>,
}

impl Default for SpyFs {
    fn default() -> Self {
        Self::new()
    }
}

impl SpyFs {
    pub fn new() -> Self {
        SpyFs {
            inner: RealFs,
            state: Mutex::new(SpyState::default()),
            now: Mutex::new(None),
        }
    }

    /// Fail the next `times` calls to `op` whose path contains
    /// `path_contains`.
    pub fn fail(&self, op: FsOp, path_contains: &str, times: usize) -> &Self {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .faults
            .push(Fault {
                op,
                path_contains: path_contains.to_owned(),
                remaining: times,
                error: InjectedError::Default,
            });
        self
    }

    /// Fail the next `times` calls to `op` whose path contains
    /// `path_contains` with a specific error (e.g. a sharing violation).
    pub fn fail_with(
        &self,
        op: FsOp,
        path_contains: &str,
        times: usize,
        error: io::Error,
    ) -> &Self {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .faults
            .push(Fault {
                op,
                path_contains: path_contains.to_owned(),
                remaining: times,
                error: InjectedError::from(&error),
            });
        self
    }

    /// Freeze the wall clock at `iso8601`.
    pub fn set_now(&self, iso8601: &str) -> &Self {
        *self.now.lock().unwrap_or_else(|error| error.into_inner()) = Some(iso8601.to_owned());
        self
    }

    /// The recorded calls, in order.
    pub fn calls(&self) -> Vec<FsCall> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .calls
            .clone()
    }

    /// Whether at least one call of `op` touched a path containing
    /// `path_contains`.
    pub fn called(&self, op: FsOp, path_contains: &str) -> bool {
        self.calls()
            .iter()
            .any(|call| call.op == op && call.path.to_string_lossy().contains(path_contains))
    }

    /// Number of calls of `op`.
    pub fn count(&self, op: FsOp) -> usize {
        self.calls().iter().filter(|call| call.op == op).count()
    }

    /// Backoff durations slept by [`rename_with_retry`].
    pub fn sleeps(&self) -> Vec<std::time::Duration> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sleeps
            .clone()
    }

    /// Clear the call log.
    pub fn reset(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .calls
            .clear();
    }

    /// Record a call and apply the first matching fault.
    fn before(&self, op: FsOp, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.calls.push(FsCall {
            op,
            path: path.to_path_buf(),
        });
        let text = path.to_string_lossy();
        let index = state.faults.iter().position(|fault| {
            fault.op == op && fault.remaining > 0 && text.contains(&fault.path_contains)
        });
        if let Some(index) = index {
            state.faults[index].remaining -= 1;
            let error = state.faults[index].make_error();
            return Err(error);
        }
        Ok(())
    }

    fn record_sleep(&self, duration: std::time::Duration) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sleeps
            .push(duration);
    }
}

macro_rules! forward {
    ($name:ident, $op:ident, $result:ty) => {
        fn $name(&self, path: &Path) -> io::Result<$result> {
            self.before(FsOp::$op, path)?;
            self.inner.$name(path)
        }
    };
}

impl UpdateFs for SpyFs {
    forward!(exists, Exists, bool);
    forward!(is_dir, IsDir, bool);
    forward!(read, Read, Vec<u8>);
    forward!(read_dir, ReadDir, Vec<FsEntry>);
    forward!(modified, Modified, SystemTime);
    forward!(create_dir_all, CreateDirAll, ());
    forward!(sha256_file, HashFile, String);
    forward!(probe_deletable, ProbeDeletable, ());
    forward!(remove_file, RemoveFile, ());
    forward!(remove_dir_all, RemoveDirAll, ());

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.before(FsOp::WriteAtomic, path)?;
        self.inner.write_atomic(path, bytes)
    }

    fn retry_sleep(&self, duration: std::time::Duration) {
        self.record_sleep(duration);
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.before(FsOp::Append, path)?;
        self.inner.append(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.before(FsOp::FileLen, path)?;
        self.inner.file_len(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        // Both ends name the step in tests: a fault matches either path.
        self.before(FsOp::Rename, to)?;
        self.before(FsOp::Rename, from)?;
        self.inner.rename(from, to)
    }

    fn now_iso8601(&self) -> String {
        self.now
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or_else(|| self.inner.now_iso8601())
    }
}

impl SpyFs {
    /// [`rename_with_retry`] over this file system, recording the sleeps
    /// instead of taking them.
    pub fn rename_with_retry(&self, from: &Path, to: &Path, retries: u32) -> io::Result<()> {
        rename_with_retry(self, from, to, retries, &|duration| {
            self.record_sleep(duration)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "rpi-wu-fs-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&root).unwrap();
            TempDir(root)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let temp = TempDir::new();
        let path = temp.0.join("state.json");
        atomic_write_bytes(&path, b"one").unwrap();
        atomic_write_bytes(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let leftovers: Vec<_> = std::fs::read_dir(&temp.0)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["state.json"]);
    }

    #[test]
    fn removals_treat_a_missing_path_as_success() {
        let fs = RealFs;
        let temp = TempDir::new();
        fs.remove_file(&temp.0.join("missing")).unwrap();
        fs.remove_dir_all(&temp.0.join("missing-dir")).unwrap();
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        let temp = TempDir::new();
        let path = temp.0.join("data");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_bytes(b"abc"), sha256_file(&path).unwrap());
    }

    #[test]
    fn rename_retry_survives_transient_failures_and_gives_up_after_the_budget() {
        let temp = TempDir::new();
        let from = temp.0.join("from");
        let to = temp.0.join("to");
        std::fs::write(&from, b"x").unwrap();
        let fs = SpyFs::new();
        fs.fail_with(FsOp::Rename, "to", 2, io::Error::from_raw_os_error(32));
        fs.rename_with_retry(&from, &to, 5).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"x");
        assert_eq!(
            fs.sleeps(),
            vec![
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(100),
            ]
        );

        let from2 = temp.0.join("from2");
        let to2 = temp.0.join("to2");
        std::fs::write(&from2, b"y").unwrap();
        fs.fail_with(FsOp::Rename, "to2", 3, io::Error::from_raw_os_error(5));
        let error = fs.rename_with_retry(&from2, &to2, 2).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(fs.sleeps().len(), 4, "two more backoffs were taken");
        assert!(from2.exists(), "a failed rename leaves the source in place");
    }

    #[test]
    fn rename_retry_does_not_retry_other_errors() {
        let temp = TempDir::new();
        let from = temp.0.join("from");
        let to = temp.0.join("to");
        std::fs::write(&from, b"x").unwrap();
        let fs = SpyFs::new();
        fs.fail(FsOp::Rename, "to", 1);
        let error = fs.rename_with_retry(&from, &to, 5).unwrap_err();
        assert!(
            error.to_string().contains("injected rename failure"),
            "{error}"
        );
        assert!(fs.sleeps().is_empty());
    }

    #[test]
    fn spy_records_calls_and_applies_faults_in_order() {
        let temp = TempDir::new();
        let fs = SpyFs::new();
        assert!(!fs.exists(&temp.0.join("nope")).unwrap());
        fs.fail(FsOp::Exists, "nope", 1);
        assert!(fs.exists(&temp.0.join("nope")).is_err());
        assert!(
            !fs.exists(&temp.0.join("nope")).unwrap(),
            "the fault is spent and the path is simply absent"
        );
        assert_eq!(fs.count(FsOp::Exists), 3);
        assert!(fs.called(FsOp::Exists, "nope"));
        fs.reset();
        assert_eq!(fs.count(FsOp::Exists), 0);
    }

    #[test]
    fn append_extends_the_log_in_place_and_reports_its_length() {
        let temp = TempDir::new();
        let path = temp.0.join("update.log");
        RealFs.append(&path, b"one\n").unwrap();
        RealFs.append(&path, b"two\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
        assert_eq!(RealFs.file_len(&path).unwrap(), 8);
        assert_eq!(RealFs.file_len(&temp.0.join("missing")).unwrap(), 0);
        let fs = SpyFs::new();
        fs.append(&path, b"three\n").unwrap();
        assert_eq!(fs.file_len(&path).unwrap(), 14);
        assert!(fs.called(FsOp::Append, "update.log"));
        assert!(fs.called(FsOp::FileLen, "update.log"));
    }

    #[test]
    fn spy_clock_can_be_frozen() {
        let fs = SpyFs::new();
        fs.set_now("2026-11-28T00:00:00Z");
        assert_eq!(fs.now_iso8601(), "2026-11-28T00:00:00Z");
    }
}
