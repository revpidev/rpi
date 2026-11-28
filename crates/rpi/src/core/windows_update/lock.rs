//! The OS-level exclusive lock of the startup transaction (design §11.5).
//!
//! `.update.lock` is opened read/write and locked exclusively through `fs2`
//! (`LockFileEx` on Windows, `flock` elsewhere). The lock is released when
//! [`UpdateLock`] drops or the process dies, so a crashed transaction never
//! wedges the next start. A start that cannot take the lock within a bound
//! continues as-is (R7.3.2): the pending is re-judged on the next start.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process held the lock for the whole wait.
    Timeout,
    /// The lock file could not be opened or locked for another reason.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Timeout => write!(f, "the update lock stayed held by another process"),
            LockError::Io(error) => write!(f, "the update lock could not be taken: {error}"),
        }
    }
}

/// A held lock. Dropping it closes the handle and releases the lock.
#[derive(Debug)]
pub struct UpdateLock {
    file: std::fs::File,
    path: PathBuf,
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl UpdateLock {
    /// The locked file path (diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `fs2` reports a held lock as `WouldBlock` on Unix and as
/// `ERROR_LOCK_VIOLATION` (33) on Windows; both mean contention.
fn is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(33)
}

/// Open `path` (creating the parent directories and the file) and lock it
/// exclusively, polling until `wait` elapses. The poll interval and the
/// clock are injectable so tests do not sleep.
pub fn acquire_exclusive(path: &Path, wait: Duration) -> Result<UpdateLock, LockError> {
    acquire_exclusive_with(
        path,
        wait,
        super::LOCK_POLL,
        &std::thread::sleep,
        &(Instant::now),
    )
}

pub fn acquire_exclusive_with(
    path: &Path,
    wait: Duration,
    poll: Duration,
    sleep: &dyn Fn(Duration),
    now: &dyn Fn() -> Instant,
) -> Result<UpdateLock, LockError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(LockError::Io)?;
    }
    let deadline = now() + wait;
    loop {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockError::Io)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                return Ok(UpdateLock {
                    file,
                    path: path.to_path_buf(),
                });
            }
            Err(error) if is_contended(&error) => {
                drop(file);
                if now() >= deadline {
                    return Err(LockError::Timeout);
                }
                sleep(poll);
            }
            Err(error) => return Err(LockError::Io(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn lock_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-wu-lock-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(".update.lock")
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let path = lock_path();
        {
            let first = acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
            assert_eq!(first.path(), path);
            // A second handle in the same process contends too (the lock
            // belongs to the open file description, not the process).
            match acquire_exclusive(&path, Duration::from_millis(60)) {
                Err(LockError::Timeout) => {}
                other => panic!("expected a timeout while the lock is held, got {other:?}"),
            }
        }
        let second = acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
        drop(second);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bounded_wait_succeeds_once_the_holder_releases() {
        let path = lock_path();
        let barrier = Arc::new(Barrier::new(2));
        let holder = {
            let barrier = Arc::clone(&barrier);
            let held_path = path.clone();
            std::thread::spawn(move || {
                let _held = acquire_exclusive(&held_path, Duration::from_millis(100)).unwrap();
                barrier.wait();
                std::thread::sleep(Duration::from_millis(80));
                // `_held` drops here: the lock is released.
            })
        };
        barrier.wait();
        let started = Instant::now();
        let acquired = acquire_exclusive(&path, Duration::from_secs(5)).unwrap();
        drop(acquired);
        holder.join().unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "the wait really blocked until the release"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_parent_directory_is_created() {
        let base = lock_path();
        let nested = base.parent().unwrap().join("nested").join(".update.lock");
        let lock = acquire_exclusive(&nested, Duration::from_millis(100)).unwrap();
        assert!(nested.is_file());
        drop(lock);
        let _ = std::fs::remove_dir_all(nested.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn zero_wait_reports_contention_as_timeout() {
        let path = lock_path();
        let held = acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
        let sleeps = std::cell::Cell::new(0);
        let error = acquire_exclusive_with(
            &path,
            Duration::ZERO,
            Duration::from_millis(1),
            &|_| sleeps.set(sleeps.get() + 1),
            &Instant::now,
        )
        .unwrap_err();
        drop(held);
        assert!(matches!(error, LockError::Timeout));
        assert_eq!(sleeps.get(), 0, "a zero wait never polls");
        assert!(error.to_string().contains("stayed held"));
        let _ = std::fs::remove_file(&path);
    }
}
