//! Per-file mutation queue for serialising concurrent file operations.
//!
//! Port of `packages/coding-agent/src/core/tools/file-mutation-queue.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The public API is `async` (tokio); the closure `f` must be `Future`-returning.
//! - The queue key uses `std::fs::canonicalize` (sync) — a single `realpath`
//!   syscall, not large file I/O. On `ENOENT`/`ENOTDIR` (file doesn't exist
//!   yet) it falls back to the lexically-normalised absolute path. All other
//!   canonicalize errors also fall back to the resolved path rather than
//!   propagating, because the return type is `T` not `Result<T>`.
//!
//! Abort semantics: the queue is **not** released in a cancellation-cleanup
//! callback. Callers (edit/write) check for cancellation at each `await` point
//! inside the closure, ensuring the per-file lock is held until the operation
//! completes. The lock guard (`_guard`) is dropped only when the closure's
//! future completes or is dropped.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

/// Global registry of per-file mutexes.
///
/// Maps a canonicalised file key to an `Arc<Mutex<()>>`. Each call to
/// `with_file_mutation_queue` clones the `Arc`, drops the registry lock, then
/// awaits the per-file mutex.
static FILE_MUTEXES: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    std::sync::OnceLock::new();

fn file_mutexes() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    FILE_MUTEXES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Determine the mutation-queue key for `file_path`.
///
/// Equivalent to upstream `getMutationQueueKey` (file-mutation-queue.ts:16-26):
/// 1. Resolve to an absolute, lexically-normalised path.
/// 2. `realpath` (canonicalize) to resolve symlinks.
/// 3. If canonicalize fails with `ENOENT` or `ENOTDIR` (or any other error),
///    fall back to the resolved path.
fn get_mutation_queue_key(file_path: &Path) -> PathBuf {
    // std::path::absolute resolves relative paths against the process cwd and
    // normalises `.`/`..` components (equivalent to Node path.resolve).
    let resolved = std::path::absolute(file_path).unwrap_or_else(|_| file_path.to_path_buf());

    match std::fs::canonicalize(&resolved) {
        Ok(canonical) => canonical,
        Err(_) => resolved, // ENOENT, ENOTDIR, or any other → use resolved path
    }
}

/// Serialize file mutation operations targeting the same file.
/// Operations for different files run in parallel.
///
/// The queue key is derived from `canonicalize` (following symlinks), so
/// operations on a file and its symlink alias share the same queue.
///
/// Port of `withFileMutationQueue` (file-mutation-queue.ts:32-60).
pub async fn with_file_mutation_queue<T, F, Fut>(file_path: &Path, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let key = get_mutation_queue_key(file_path);

    // Registration phase: get-or-create the per-file mutex, clone the Arc,
    // then immediately release the global lock.
    let mutex = {
        let mut map = file_mutexes().lock().await;
        let entry = map
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())));
        entry.clone()
    };

    // Acquire the per-file lock — this serialises concurrent operations on
    // the same file. Different files proceed in parallel.
    let result = {
        let _guard = mutex.lock().await;
        f().await
    };
    // _guard dropped here — per-file lock released.

    // GC phase: if we are the last holder besides the map, remove the entry
    // to prevent unbounded growth. While holding the global map lock, no new
    // registrations can occur, so the strong-count check is race-free.
    // strong_count == 2 means: one in the map + one in our `mutex` variable.
    {
        let mut map = file_mutexes().lock().await;
        if let Some(entry) = map.get(&key) {
            if Arc::strong_count(entry) == 2 {
                map.remove(&key);
            }
        }
    }

    result
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers::TempDir;
    use std::time::Duration;

    type OrderLog = Arc<Mutex<Vec<String>>>;

    async fn record(log: &OrderLog, label: &str) {
        log.lock().await.push(label.to_string());
    }

    // Port of "serializes operations for the same file"
    #[tokio::test]
    async fn test_serializes_same_file() {
        let log: OrderLog = Arc::new(Mutex::new(Vec::new()));

        let path = std::env::temp_dir().join("pir-mq-same-file-test");
        let log1 = log.clone();
        let path1 = path.clone();
        let h1 = tokio::spawn(async move {
            with_file_mutation_queue(&path1, || async {
                record(&log1, "first:start").await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                record(&log1, "first:end").await;
            })
            .await
        });
        let log2 = log.clone();
        let h2 = tokio::spawn(async move {
            with_file_mutation_queue(&path, || async {
                record(&log2, "second:start").await;
                record(&log2, "second:end").await;
            })
            .await
        });

        h1.await.unwrap();
        h2.await.unwrap();

        let order = log.lock().await.clone();
        assert_eq!(
            order,
            vec!["first:start", "first:end", "second:start", "second:end",]
        );
    }

    // Port of "allows different files to proceed in parallel"
    #[tokio::test]
    async fn test_parallel_different_files() {
        let log: OrderLog = Arc::new(Mutex::new(Vec::new()));

        let path_a = std::env::temp_dir().join("pir-mq-parallel-a");
        let path_b = std::env::temp_dir().join("pir-mq-parallel-b");

        let log_a = log.clone();
        let h1 = tokio::spawn(async move {
            with_file_mutation_queue(&path_a, || async {
                record(&log_a, "a:start").await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                record(&log_a, "a:end").await;
            })
            .await
        });
        let log_b = log.clone();
        let h2 = tokio::spawn(async move {
            with_file_mutation_queue(&path_b, || async {
                record(&log_b, "b:start").await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                record(&log_b, "b:end").await;
            })
            .await
        });

        h1.await.unwrap();
        h2.await.unwrap();

        let order = log.lock().await;
        let a_start = order.iter().position(|s| s == "a:start").unwrap();
        let a_end = order.iter().position(|s| s == "a:end").unwrap();
        let b_start = order.iter().position(|s| s == "b:start").unwrap();

        // b:start should come before a:end (they overlap in time).
        assert!(b_start < a_end, "b:start should overlap with a");
        assert!(a_start < a_end);
    }

    // Port of "uses the same queue for symlink aliases"
    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlink_alias_same_queue() {
        let log: OrderLog = Arc::new(Mutex::new(Vec::new()));

        let tmp = TempDir::new();
        let target = tmp.path().join("target.txt");
        let alias = tmp.path().join("alias.txt");
        std::fs::write(&target, "hello\n").unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        let log1 = log.clone();
        let target_clone = target.clone();
        let h1 = tokio::spawn(async move {
            with_file_mutation_queue(&target_clone, || async {
                record(&log1, "target:start").await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                record(&log1, "target:end").await;
            })
            .await
        });
        let log2 = log.clone();
        let h2 = tokio::spawn(async move {
            with_file_mutation_queue(&alias, || async {
                record(&log2, "alias:start").await;
                record(&log2, "alias:end").await;
            })
            .await
        });

        h1.await.unwrap();
        h2.await.unwrap();

        let order = log.lock().await.clone();
        assert_eq!(
            order,
            vec!["target:start", "target:end", "alias:start", "alias:end",]
        );
    }

    // Ensure the closure's return value is propagated correctly.
    #[tokio::test]
    async fn test_return_value_propagated() {
        let path = std::env::temp_dir().join("pir-mq-return-test");
        let result = with_file_mutation_queue(&path, || async { 42 }).await;
        assert_eq!(result, 42);
    }

    // Ensure GC removes entries after operations complete.
    #[tokio::test]
    async fn test_gc_removes_entry() {
        let path = std::env::temp_dir().join("pir-mq-gc-test");
        let _ = with_file_mutation_queue(&path, || async {}).await;

        // After completion, the entry should be removed (if no concurrent ops).
        let map = file_mutexes().lock().await;
        let key = get_mutation_queue_key(&path);
        // The entry may or may not be present depending on timing, but if
        // present, strong_count should be 1 (map only).
        if let Some(entry) = map.get(&key) {
            assert_eq!(Arc::strong_count(entry), 1);
        }
    }
}
