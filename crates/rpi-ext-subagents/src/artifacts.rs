//! Artifact directory selection, file naming, metadata persistence and the
//! 24h-throttled age cleanup (FR-P0-09).
//!
//! Port of pi-subagents `src/shared/artifacts.ts` @ v0.48.0 (56f97234) plus
//! `DEFAULT_ARTIFACT_CONFIG` / `resolveTempScopeId` from types.ts. The npm
//! packaging warning (`getProjectArtifactPackagingWarning`) is not ported: it
//! inspects package.json/.npmignore for npm publish leakage, which has no
//! rpi equivalent (deviation TE-D18).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::paths;

pub const CLEANUP_MARKER_FILE: &str = ".last-cleanup";
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactDirPreference {
    Project,
    Session,
    Temp,
}

impl ArtifactDirPreference {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            Some("project") => Ok(Self::Project),
            Some("session") => Ok(Self::Session),
            Some("temp") => Ok(Self::Temp),
            other => Err(format!(
                "Unsupported artifactDir {:?}; expected \"project\", \"session\", or \"temp\".",
                other
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactPaths {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub jsonl_path: PathBuf,
    pub transcript_path: PathBuf,
    pub metadata_path: PathBuf,
}

impl ArtifactPaths {
    pub fn to_json(&self) -> Value {
        json!({
            "inputPath": self.input_path.to_string_lossy(),
            "outputPath": self.output_path.to_string_lossy(),
            "jsonlPath": self.jsonl_path.to_string_lossy(),
            "transcriptPath": self.transcript_path.to_string_lossy(),
            "metadataPath": self.metadata_path.to_string_lossy(),
        })
    }
}

/// `getArtifactsDir` (artifacts.ts:160-184).
pub fn get_artifacts_dir(
    session_file: Option<&Path>,
    project_cwd: Option<&Path>,
    preference: ArtifactDirPreference,
) -> PathBuf {
    let session_artifacts = |session_file: Option<&Path>| match session_file {
        Some(file) => file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("subagent-artifacts"),
        None => paths::temp_artifacts_dir(),
    };
    match preference {
        ArtifactDirPreference::Session => session_artifacts(session_file),
        ArtifactDirPreference::Temp => paths::temp_artifacts_dir(),
        ArtifactDirPreference::Project => match project_cwd {
            Some(cwd) => paths::get_project_artifacts_dir(cwd),
            None => session_artifacts(session_file),
        },
    }
}

/// `getArtifactPaths` (artifacts.ts:186-197). `safe_agent` keeps `[^\w.-]` → `_`.
pub fn get_artifact_paths(
    artifacts_dir: &Path,
    run_id: &str,
    agent: &str,
    index: Option<u32>,
) -> ArtifactPaths {
    let safe_agent: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = match index {
        Some(index) => format!("{run_id}_{safe_agent}_{index}"),
        None => format!("{run_id}_{safe_agent}"),
    };
    ArtifactPaths {
        input_path: artifacts_dir.join(format!("{base}_input.md")),
        output_path: artifacts_dir.join(format!("{base}_output.md")),
        jsonl_path: artifacts_dir.join(format!("{base}.jsonl")),
        transcript_path: artifacts_dir.join(format!("{base}_transcript.jsonl")),
        metadata_path: artifacts_dir.join(format!("{base}_meta.json")),
    }
}

pub fn ensure_artifacts_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// `writeArtifact` (artifacts.ts:203-206) — creates parents, plain write.
pub fn write_artifact(file_path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file_path, content)
}

/// `formatOutputArtifactContent` (artifacts.ts:208-219).
pub fn format_output_artifact_content(
    output: &str,
    error: Option<&str>,
    transcript_path: Option<&Path>,
    metadata_path: Option<&Path>,
) -> String {
    if !output.trim().is_empty() || error.is_none() {
        return output.to_string();
    }
    let mut lines = vec![
        "Subagent run failed before producing output.".to_string(),
        String::new(),
        format!("Error:{}", error.unwrap_or("")),
    ];
    if let Some(transcript) = transcript_path {
        lines.push(String::new());
        lines.push(format!("Transcript: {}", transcript.to_string_lossy()));
    }
    if let Some(metadata) = metadata_path {
        lines.push(format!("Metadata: {}", metadata.to_string_lossy()));
    }
    lines.join("\n")
}

/// Retryable errno classes for artifact metadata writes (R7.1.7.1, #1227/
/// #1272): storage capacity (`ENOSPC`/`EDQUOT`, fd exhaustion `EMFILE`/
/// `ENFILE`) plus transient locks (`EBUSY`/`EAGAIN`). Permission errors
/// (`EACCES`/`EPERM`) and missing paths (`ENOENT`) never retry — retrying
/// cannot fix them and would only stall the terminal path. This splits the
/// upstream classes (file-system-retry.ts:3-4 @ 0fc0eebb retriess
/// lock-class synchronously, defers capacity-class via a pending queue)
/// into one bounded synchronous ladder, per design 04 §3.3.6.
#[cfg(unix)]
pub fn is_retryable_write_error(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(code) => matches!(
            code,
            libc::ENOSPC | libc::EDQUOT | libc::EMFILE | libc::ENFILE | libc::EBUSY | libc::EAGAIN
        ),
        None => false,
    }
}

/// Non-unix targets have no errno mapping to match — treat every write
/// failure as terminal (conservative: no retry, callers see the error).
#[cfg(not(unix))]
pub fn is_retryable_write_error(_error: &std::io::Error) -> bool {
    false
}

/// Total attempts of the bounded write retry (attempt 1 + 4 retries).
pub const WRITE_METADATA_MAX_ATTEMPTS: u32 = 5;
/// Exponential backoff base: sleep `base << (attempt - 1)` before retry
/// attempt N+1 → 20/40/80/160 ms, ≤ 300 ms total sleep per call.
pub const WRITE_METADATA_BACKOFF_BASE_MS: u64 = 20;

/// Backoff before retry attempt `attempt + 1` (attempt is 1-based).
pub(crate) fn write_retry_backoff_ms(attempt: u32) -> u64 {
    const MAX_SHIFT: u32 = 8;
    WRITE_METADATA_BACKOFF_BASE_MS << (attempt - 1).min(MAX_SHIFT)
}

/// Attempt count a caller may attribute to a failed write: retryable
/// errors ran the full ladder, everything else failed on the first try.
pub(crate) fn write_attempts_for(error: &std::io::Error) -> u32 {
    if is_retryable_write_error(error) {
        WRITE_METADATA_MAX_ATTEMPTS
    } else {
        1
    }
}

/// `writeMetadata` (artifacts.ts:221-224; v0.66 atomic-json.ts:59-77) —
/// two-space indented JSON, written atomically (tmp file + rename,
/// upstream `writeAtomicJson`): concurrent readers never observe a torn
/// document and multi-writer races resolve to one whole file. Retryable
/// failures (capacity/transient lock) retry with exponential backoff up to
/// [`WRITE_METADATA_MAX_ATTEMPTS`] total attempts (R7.1.7.1); the final
/// error propagates so callers can surface a diagnostic event.
pub fn write_metadata(file_path: &Path, metadata: &Value) -> std::io::Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(metadata).unwrap_or_default();
    let result = write_metadata_retrying(file_path, &body, |tmp, target, body| {
        std::fs::write(tmp, body).and_then(|()| std::fs::rename(tmp, target))
    });
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        WRITE_METADATA_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// The bounded retry ladder behind [`write_metadata`] (R7.1.7.1). The
/// `attempt_io` closure performs one whole tmp-write + rename attempt;
/// production passes the real filesystem calls, tests inject deterministic
/// failures (no disk-full or lock orchestration needed). Each failed
/// attempt removes its tmp file; a retryable error with attempts left logs
/// a warning (path, error kind, attempt) and backs off before the next try.
pub(crate) fn write_metadata_retrying<F>(
    file_path: &Path,
    body: &str,
    attempt_io: F,
) -> std::io::Result<()>
where
    F: Fn(&Path, &Path, &str) -> std::io::Result<()>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let tmp =
            file_path.with_extension(format!("tmp-{}", std::process::id() as u64 ^ nanos_now()));
        // Same-directory rename is atomic on unix; a failed attempt (write
        // or rename) cleans its tmp file and either retries or propagates.
        match attempt_io(&tmp, file_path, body) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(&tmp);
                if attempt >= WRITE_METADATA_MAX_ATTEMPTS || !is_retryable_write_error(&error) {
                    return Err(error);
                }
                tracing::warn!(
                    path = %file_path.display(),
                    kind = ?error.kind(),
                    attempt,
                    error = %error,
                    "metadata write failed with a retryable error; backing off"
                );
                std::thread::sleep(std::time::Duration::from_millis(write_retry_backoff_ms(
                    attempt,
                )));
            }
        }
    }
}

/// Test-only counter of `write_metadata` calls (V13-01 FR-C quantitative
/// acceptance: status write coalescing assertion).
#[cfg(test)]
pub(crate) static WRITE_METADATA_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
fn nanos_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn append_jsonl(file_path: &Path, line: &str) {
    use std::io::Write;
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Cumulative ceiling for the persistent JSONL writers — same value as
/// upstream `DEFAULT_MAX_JSONL_BYTES` (jsonl-writer.ts:14).
pub const DEFAULT_MAX_JSONL_BYTES: u64 = 50 * 1024 * 1024;

/// Persistent append-only JSONL writer (V13-01 FR-A/FR-B): the port of
/// upstream `JsonlWriter` (jsonl-writer.ts:28-89) — ONE open append handle
/// per child run instead of the previous per-line open/close+mkdir, with a
/// 50MiB cumulative ceiling (DEFAULT_MAX_JSONL_BYTES) past which subsequent
/// lines are silently dropped.
///
/// Semantics mirror upstream `writeLine` (jsonl-writer.ts:58-77): blank
/// lines are skipped; write failures are best-effort silent (disk-full etc.)
/// and later lines keep trying (R4). The reader-side backpressure (upstream
/// pause/resume on the child stream) is intentionally NOT ported: rpi's
/// line loop is itself the consumer and BoundedLineReader already caps the
/// line size — see deviation TE-D35.
///
/// Construction is the only place `create_dir_all` runs; the writer is pure
/// synchronous IO (same execution environment as the previous
/// `append_jsonl` per-line calls — no new Send constraints).
pub struct JsonlWriter {
    /// `None` = not created / unavailable / ceiling retired.
    file: Option<std::fs::File>,
    bytes_written: u64,
    max_bytes: u64,
}

/// Test-only counter of `JsonlWriter::create` calls (V13-01 FR-A
/// quantitative acceptance: open count == 1 per run).
#[cfg(test)]
pub(crate) static JSONL_CREATE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl JsonlWriter {
    /// Create the writer: parent `create_dir_all` + open-append exactly once.
    pub fn create(path: &Path, max_bytes: u64) -> Self {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            JSONL_CREATE_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        let file = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
            .ok()
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            });
        Self {
            file,
            bytes_written: 0,
            max_bytes,
        }
    }

    /// Append one JSONL line (upstream `writeLine`): blank skip, ceiling
    /// drop, best-effort failures.
    pub fn write_line(&mut self, line: &str) {
        let Some(file) = &mut self.file else {
            return; // unavailable or ceiling-retired
        };
        if line.trim().is_empty() {
            return;
        }
        use std::io::Write;
        let bytes = line.len() as u64 + 1; // + trailing newline
        if self.max_bytes > 0 && self.bytes_written + bytes > self.max_bytes {
            // Ceiling reached: retire the handle and silently drop the line
            // (upstream drops subsequent lines without erroring).
            self.file = None;
            return;
        }
        if writeln!(file, "{line}").is_ok() {
            self.bytes_written += bytes;
        }
        // A failed write leaves the handle in place; later lines retry
        // (best-effort, FR-A R4).
    }
}

/// `cleanupOldArtifacts` (artifacts.ts:230-259): 24h-throttled via the
/// `.last-cleanup` marker, first directory level only, best-effort unlinks.
pub fn cleanup_old_artifacts(dir: &Path, max_age_days: u64) {
    if max_age_days == 0 || !dir.exists() {
        return;
    }
    let marker_path = dir.join(CLEANUP_MARKER_FILE);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if let Ok(meta) = std::fs::metadata(&marker_path)
        && let Ok(modified) = meta.modified()
    {
        let mtime = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if (now as u128) < mtime + DAY_MS as u128 {
            return;
        }
    }
    let cutoff = now.saturating_sub(max_age_days * DAY_MS);
    // V19-03 S33 (#3217): recurse into nested artifact folders (workflow
    // child outputs under `outputs/`) with a safe-root walker; worktree
    // handoff records/patches are preserved, symlinks are never followed,
    // and a path that resolves outside the root is skipped.
    let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    cleanup_dir_recursive(dir, &root, cutoff);
    let _ = std::fs::write(&marker_path, now.to_string());
}

/// `cleanupOldArtifacts` recursion (V19-03 S33): one safe-root walk. Keeps
/// `handoffs/` and `*.patch` files (worktree recovery evidence), never
/// follows symlinks, and only removes entries whose real path stays under
/// the root.
fn cleanup_dir_recursive(dir: &Path, root: &Path, cutoff: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == CLEANUP_MARKER_FILE {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(root) {
            continue;
        }
        if file_type.is_dir() {
            if name == "handoffs" {
                continue;
            }
            cleanup_dir_recursive(&path, root, cutoff);
            let empty = std::fs::read_dir(&path)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if empty {
                let _ = std::fs::remove_dir(&path);
            }
            continue;
        }
        if name.ends_with(".patch") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let mtime = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if mtime < cutoff {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Fork session copies under `<agent sessions>/<project>/<session>/forks/`
/// (V19-03 S42, #3232): delete a copy only when no active background run
/// references it and nothing wrote to it for `cleanup_days`. Mirrors upstream
/// `removeOldForkCopies` (artifacts.ts:310-380 @ v0.88.1): two directory
/// levels (project → session), a `forks` directory that must not be a
/// symlink, and per-run directories. Explicit `sessionDir` copies live under
/// a user-owned root and are intentionally never swept here.
pub fn cleanup_fork_copies(cleanup_days: u64) {
    if cleanup_days == 0 {
        return;
    }
    let sessions_base = paths::get_agent_dir().join("sessions");
    let active = active_fork_runs();
    cleanup_fork_copies_at(&sessions_base, cleanup_days, &active);
}

/// Active-run protection for the fork sweep: the run ids that are still
/// queued/running/paused (upstream `asyncRunActive`) plus the session files
/// they already recorded (a resumed run may write an older copy).
#[derive(Debug, Default)]
struct ActiveForkRuns {
    run_ids: BTreeSet<String>,
    session_files: Vec<PathBuf>,
}

fn cleanup_fork_copies_at(sessions_base: &Path, cleanup_days: u64, active: &ActiveForkRuns) {
    if cleanup_days == 0 {
        return;
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(cleanup_days * 24 * 60 * 60))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let Ok(project_dirs) = std::fs::read_dir(sessions_base) else {
        return;
    };
    for project_dir in project_dirs.flatten() {
        // `DirEntry::file_type` does not follow symlinks, matching upstream's
        // `Dirent.isDirectory()`: a symlinked project/session dir is skipped.
        if !project_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(session_dirs) = std::fs::read_dir(project_dir.path()) else {
            continue;
        };
        for session_dir in session_dirs.flatten() {
            if !session_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let forks = session_dir.path().join("forks");
            // Never follow a symlinked `forks` folder: the deletes below would
            // land in its target (upstream `lstatSync(forksDir).isDirectory()`).
            let Ok(forks_meta) = std::fs::symlink_metadata(&forks) else {
                continue;
            };
            if !forks_meta.is_dir() || forks_meta.file_type().is_symlink() {
                continue;
            }
            let Ok(run_dirs) = std::fs::read_dir(&forks) else {
                continue;
            };
            let root = forks.canonicalize().unwrap_or_else(|_| forks.clone());
            for run_dir in run_dirs.flatten() {
                if !run_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let run_id = run_dir.file_name().to_string_lossy().to_string();
                if active.run_ids.contains(&run_id) {
                    continue;
                }
                let path = run_dir.path();
                let Ok(canonical) = path.canonicalize() else {
                    continue;
                };
                if !canonical.starts_with(&root) {
                    continue;
                }
                if active
                    .session_files
                    .iter()
                    .any(|active| canonical.starts_with(active) || active.starts_with(&canonical))
                {
                    continue;
                }
                let newest = newest_write(&path);
                if newest.is_some_and(|newest| newest < cutoff) {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }
}

/// Newest mtime anywhere below `path` (the copy itself when it is a file).
fn newest_write(path: &Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return meta.modified().ok();
    }
    let mut newest = meta.modified().ok();
    let Ok(entries) = std::fs::read_dir(path) else {
        return newest;
    };
    for entry in entries.flatten() {
        if entry
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(true)
        {
            continue;
        }
        if let Some(candidate) = newest_write(&entry.path())
            && newest.is_none_or(|current| candidate > current)
        {
            newest = Some(candidate);
        }
    }
    newest
}

/// Session files and run ids of non-terminal async runs (fork-copy
/// protection). The run id is checked first: a queued run may not have
/// recorded its fork session file yet, and upstream protects by run id.
fn active_fork_runs() -> ActiveForkRuns {
    active_fork_runs_in(&crate::runner::background::async_runs_dir())
}

/// `asyncRunActive` (artifacts.ts:363-372 @ v0.88.1): a missing status is not
/// active, but an unreadable or unparseable status may still belong to a live
/// run, so it protects the copy (upstream throws and the catch returns true).
fn active_fork_runs_in(runs: &Path) -> ActiveForkRuns {
    let mut active = ActiveForkRuns::default();
    let Ok(entries) = std::fs::read_dir(runs) else {
        return active;
    };
    for entry in entries.flatten() {
        let run_id = entry.file_name().to_string_lossy().to_string();
        let status_path = entry.path().join("status.json");
        let raw = match std::fs::read_to_string(&status_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                active.run_ids.insert(run_id);
                continue;
            }
        };
        let status = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(status) => status,
            Err(_) => {
                active.run_ids.insert(run_id);
                continue;
            }
        };
        if !matches!(
            status["state"].as_str().unwrap_or(""),
            "queued" | "running" | "paused"
        ) {
            continue;
        }
        active.run_ids.insert(run_id);
        for step in status["steps"].as_array().into_iter().flatten() {
            if let Some(file) = step["sessionFile"].as_str()
                && let Ok(path) = PathBuf::from(file).canonicalize()
            {
                active.session_files.push(path);
            }
        }
    }
    active
}

/// `cleanupAllArtifactDirs` (artifacts.ts:261-285): temp artifacts + every
/// `<agentDir>/sessions/*/subagent-artifacts`.
pub fn cleanup_all_artifact_dirs(max_age_days: u64) {
    cleanup_old_artifacts(&paths::temp_artifacts_dir(), max_age_days);
    // Chain scratch dirs: user-scoped temp roots get the 24h sweep
    // (settings.ts cleanupOldChainDirs L197-215); project-local roots are
    // never age-scanned, matching upstream.
    crate::p1::chain::cleanup_old_chain_dirs(
        &paths::temp_root_dir().join("chain-runs"),
        crate::p1::chain::CHAIN_DIR_MAX_AGE_MS,
    );
    let sessions_base = paths::get_agent_dir().join("sessions");
    let Ok(dirs) = std::fs::read_dir(&sessions_base) else {
        return;
    };
    for dir in dirs.flatten() {
        let artifacts_dir = dir.path().join("subagent-artifacts");
        cleanup_old_artifacts(&artifacts_dir, max_age_days);
    }
}

/// RFC3339 UTC timestamp with milliseconds, the rpi session/`timestamp`
/// convention (`new Date().toISOString()` upstream).
pub fn format_iso8601(unix_millis: u64) -> String {
    let seconds = unix_millis / 1000;
    let millis = unix_millis % 1000;
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u64, d as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S42 cleanup tests. They backdate directory mtimes, which the portable
    /// `File::set_modified` path only supports on Unix; Windows keeps the
    /// compile probe and the layout/unit tests.
    #[cfg(unix)]
    mod fork_cleanup {
        use super::*;

        fn backdate(path: &Path, age_secs: u64) {
            let modified = std::time::SystemTime::now()
                .checked_sub(std::time::Duration::from_secs(age_secs))
                .unwrap();
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap().flatten() {
                    backdate(&entry.path(), age_secs);
                }
            }
            let file = std::fs::File::open(path).unwrap();
            file.set_modified(modified).unwrap();
        }

        /// `<base>/<project>/<session>/forks/<run>/fork.jsonl` with a backdated
        /// tree, matching the writer layout (`fork_session_root`).
        fn fork_run(
            base: &Path,
            project: &str,
            session: &str,
            run: &str,
            age_secs: u64,
        ) -> PathBuf {
            let run_dir = base.join(project).join(session).join("forks").join(run);
            std::fs::create_dir_all(&run_dir).unwrap();
            let file = run_dir.join("fork.jsonl");
            std::fs::write(&file, "{\"type\":\"session\"}\n").unwrap();
            backdate(&file, age_secs);
            backdate(&run_dir, age_secs);
            run_dir
        }

        /// S42: the sweep walks the real two-level layout and removes only idle
        /// fork run dirs; regular `run-N/` child sessions are never touched.
        #[test]
        fn fork_cleanup_removes_only_idle_run_dirs() {
            let base =
                std::env::temp_dir().join(format!("rpi-fork-cleanup-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let old = fork_run(&base, "--proj--", "parent-stem", "old-run", 8 * 86_400);
            let fresh = fork_run(&base, "--proj--", "parent-stem", "fresh-run", 0);
            let regular = base
                .join("--proj--")
                .join("parent-stem")
                .join("run-0")
                .join("session.jsonl");
            std::fs::create_dir_all(regular.parent().unwrap()).unwrap();
            std::fs::write(&regular, "{}\n").unwrap();
            backdate(&regular, 30 * 86_400);
            backdate(regular.parent().unwrap(), 30 * 86_400);

            cleanup_fork_copies_at(&base, 7, &ActiveForkRuns::default());

            assert!(
                !old.exists(),
                "idle fork run older than cleanupDays is removed"
            );
            assert!(fresh.exists(), "recently written fork run is kept");
            assert!(regular.exists(), "a regular child session is never swept");
            let _ = std::fs::remove_dir_all(&base);
        }

        /// S42: an active run id protects a fork run even before it records its
        /// session file (queued run), and a recorded session file protects a
        /// resumed copy under another run id.
        #[test]
        fn fork_cleanup_skips_active_run_ids_and_session_files() {
            let base = std::env::temp_dir().join(format!("rpi-fork-active-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let by_id = fork_run(&base, "--proj--", "parent-stem", "queued-run", 8 * 86_400);
            let by_file = fork_run(&base, "--proj--", "parent-stem", "resumed-run", 8 * 86_400);
            let stale = fork_run(&base, "--proj--", "parent-stem", "stale-run", 8 * 86_400);
            let mut active = ActiveForkRuns::default();
            active.run_ids.insert("queued-run".to_string());
            active
                .session_files
                .push(by_file.join("fork.jsonl").canonicalize().unwrap());

            cleanup_fork_copies_at(&base, 7, &active);

            assert!(by_id.exists(), "queued run id keeps its fork copy");
            assert!(
                by_file.exists(),
                "a recorded active session file keeps its copy"
            );
            assert!(!stale.exists(), "unreferenced idle copies are removed");
            let _ = std::fs::remove_dir_all(&base);
        }

        /// S42/upstream `asyncRunActive`: a status that exists but cannot be
        /// read or parsed may still belong to a live run, so it protects the
        /// copy; a missing status is not active.
        #[test]
        fn fork_cleanup_treats_an_unreadable_status_as_active() {
            let base = std::env::temp_dir().join(format!("rpi-fork-status-{}", std::process::id()));
            let runs = std::env::temp_dir().join(format!("rpi-fork-runs-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&runs);
            let broken = fork_run(
                &base,
                "--proj--",
                "parent-stem",
                "broken-status",
                30 * 86_400,
            );
            let missing = fork_run(
                &base,
                "--proj--",
                "parent-stem",
                "missing-status",
                30 * 86_400,
            );
            std::fs::create_dir_all(runs.join("broken-status")).unwrap();
            std::fs::write(runs.join("broken-status").join("status.json"), "{not json").unwrap();
            // `missing-status` has no run folder at all: not active.
            let active = active_fork_runs_in(&runs);
            cleanup_fork_copies_at(&base, 7, &active);
            assert!(broken.exists(), "an unreadable status protects the copy");
            assert!(!missing.exists(), "a missing status is not active");
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&runs);
        }

        /// `cleanupDays: 0` disables the sweep (upstream `cleanupDays` gate).
        #[test]
        fn fork_cleanup_zero_days_is_a_noop() {
            let base = std::env::temp_dir().join(format!("rpi-fork-zero-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let old = fork_run(&base, "--proj--", "parent-stem", "old-run", 30 * 86_400);
            cleanup_fork_copies_at(&base, 0, &ActiveForkRuns::default());
            assert!(old.exists());
            let _ = std::fs::remove_dir_all(&base);
        }

        /// S42 safety: a symlinked `forks` directory is never followed, so the
        /// deletes cannot land in its target.
        #[test]
        fn fork_cleanup_does_not_follow_a_symlinked_forks_dir() {
            let base =
                std::env::temp_dir().join(format!("rpi-fork-symlink-{}", std::process::id()));
            let outside =
                std::env::temp_dir().join(format!("rpi-fork-target-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&outside);
            let session = base.join("--proj--").join("parent-stem");
            std::fs::create_dir_all(&session).unwrap();
            let target = fork_run(&outside, "target", "forks", "old-run", 30 * 86_400);
            #[cfg(unix)]
            std::os::unix::fs::symlink(outside.join("target").join("forks"), session.join("forks"))
                .unwrap();
            #[cfg(windows)]
            std::os::windows::fs::symlink_dir(
                outside.join("target").join("forks"),
                session.join("forks"),
            )
            .unwrap();

            cleanup_fork_copies_at(&base, 7, &ActiveForkRuns::default());

            assert!(target.exists(), "a symlinked forks dir is not swept");
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&outside);
        }

        /// S42 safety: symlinked project/session directories are skipped the same
        /// way upstream's `Dirent.isDirectory()` skips them.
        #[test]
        fn fork_cleanup_skips_symlinked_project_and_session_dirs() {
            let base =
                std::env::temp_dir().join(format!("rpi-fork-dirlink-{}", std::process::id()));
            let outside = std::env::temp_dir()
                .join(format!("rpi-fork-dirlink-target-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&outside);
            std::fs::create_dir_all(&base).unwrap();
            let target = fork_run(&outside, "--proj--", "parent-stem", "old-run", 30 * 86_400);
            #[cfg(unix)]
            std::os::unix::fs::symlink(outside.join("--proj--"), base.join("--proj--")).unwrap();
            #[cfg(windows)]
            std::os::windows::fs::symlink_dir(outside.join("--proj--"), base.join("--proj--"))
                .unwrap();

            cleanup_fork_copies_at(&base, 7, &ActiveForkRuns::default());

            assert!(target.exists(), "a symlinked project dir is not swept");
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&outside);
        }

        /// S42 age rule: one fresh write anywhere below the run dir keeps the
        /// whole copy (upstream `idleSessionFiles`).
        #[test]
        fn fork_cleanup_keeps_a_run_with_one_fresh_nested_write() {
            let base = std::env::temp_dir().join(format!("rpi-fork-nested-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let run = fork_run(&base, "--proj--", "parent-stem", "old-run", 30 * 86_400);
            let nested = run.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(nested.join("fork-1.jsonl"), "{}\n").unwrap();

            cleanup_fork_copies_at(&base, 7, &ActiveForkRuns::default());

            assert!(run.exists(), "a fresh nested write keeps the copy");
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn artifact_paths_naming_and_sanitization() {
        let dir = Path::new("/work/.rpi/subagents/artifacts");
        let paths = get_artifact_paths(dir, "ab12cd34", "scout", Some(0));
        assert_eq!(
            paths.input_path.file_name().unwrap(),
            "ab12cd34_scout_0_input.md"
        );
        assert_eq!(
            paths.output_path.file_name().unwrap(),
            "ab12cd34_scout_0_output.md"
        );
        assert_eq!(
            paths.jsonl_path.file_name().unwrap(),
            "ab12cd34_scout_0.jsonl"
        );
        assert_eq!(
            paths.transcript_path.file_name().unwrap(),
            "ab12cd34_scout_0_transcript.jsonl"
        );
        assert_eq!(
            paths.metadata_path.file_name().unwrap(),
            "ab12cd34_scout_0_meta.json"
        );
        let paths = get_artifact_paths(dir, "ab12cd34", "weird/name!", None);
        assert_eq!(
            paths.output_path.file_name().unwrap(),
            "ab12cd34_weird_name__output.md"
        );
    }

    #[test]
    fn dir_preference_fallbacks() {
        // project without cwd falls back to session, then temp
        assert_eq!(
            get_artifacts_dir(
                Some(Path::new("/s/x.jsonl")),
                None,
                ArtifactDirPreference::Project
            ),
            Path::new("/s/subagent-artifacts")
        );
        assert_eq!(
            get_artifacts_dir(None, None, ArtifactDirPreference::Project),
            paths::temp_artifacts_dir()
        );
        assert_eq!(
            get_artifacts_dir(
                None,
                Some(Path::new("/repo")),
                ArtifactDirPreference::Project
            ),
            Path::new("/repo/.rpi/subagents/artifacts")
        );
    }

    #[test]
    fn cleanup_respects_marker_throttle_and_age() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.jsonl");
        std::fs::write(&old, "x").unwrap();
        // Backdate beyond cleanupDays and the marker window.
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400);
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(stale).unwrap();
        drop(file);
        cleanup_old_artifacts(&dir, 7);
        assert!(!old.exists(), "aged artifact removed");
        assert!(dir.join(CLEANUP_MARKER_FILE).exists(), "marker written");
        // Second call inside 24h is a no-op — a fresh aged file survives.
        let old2 = dir.join("old2.jsonl");
        std::fs::write(&old2, "x").unwrap();
        let file = std::fs::File::options().write(true).open(&old2).unwrap();
        file.set_modified(stale).unwrap();
        drop(file);
        cleanup_old_artifacts(&dir, 7);
        assert!(old2.exists(), "throttled by 24h marker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_format_matches_rfc3339() {
        assert_eq!(format_iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_iso8601(1_755_168_000_123),
            "2025-08-14T10:40:00.123Z"
        );
    }

    // ---- V13-01: JsonlWriter persistent writer (FR-A) ----------------

    /// JSONL_CREATE_COUNT is process-global; all JsonlWriter tests in this
    /// module serialize so their counter deltas are exclusive.
    static JSONL_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn jsonl_writer_appends_and_counts_open_once() {
        let _guard = JSONL_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rpi-sub-jsonl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("events.jsonl");
        let before = JSONL_CREATE_COUNT.load(std::sync::atomic::Ordering::Relaxed);
        {
            let mut writer = JsonlWriter::create(&path, DEFAULT_MAX_JSONL_BYTES);
            for i in 0..200 {
                writer.write_line(&format!(r#"{{"type":"event","i":{i}}}"#));
            }
        }
        let after = JSONL_CREATE_COUNT.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after - before, 1, "one create (one open) for the whole run");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 200, "all lines persisted");
        assert!(content.contains(r#""i":199"#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonl_writer_skips_blank_lines_and_survives_empty_input() {
        let _guard = JSONL_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rpi-sub-jsonl2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("events.jsonl");
        let mut writer = JsonlWriter::create(&path, DEFAULT_MAX_JSONL_BYTES);
        writer.write_line("");
        writer.write_line("   ");
        writer.write_line(r#"{"type":"a"}"#);
        writer.write_line("\t");
        drop(writer);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.lines().count(),
            1,
            "only the non-blank line persisted"
        );
        assert!(content.contains("a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonl_writer_silently_drops_beyond_ceiling() {
        let _guard = JSONL_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rpi-sub-jsonl3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("events.jsonl");
        // 64-byte ceiling → a handful of lines fit, the rest are dropped
        // silently (upstream jsonl-writer.ts:58-77 parity).
        let mut writer = JsonlWriter::create(&path, 64);
        for _ in 0..100 {
            writer.write_line(r#"{"type":"payload","body":"xxxx"}"#);
        }
        drop(writer);
        let content = std::fs::read_to_string(&path).unwrap();
        let lines = content.lines().count();
        assert!(
            (1..100).contains(&lines),
            "line count under the ceiling, not all 100: {lines}"
        );
        assert!(
            lines <= 3,
            "64-byte ceiling leaves fewer than 4 full lines: {lines}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonl_writer_unwritable_dir_is_best_effort_silent() {
        let _guard = JSONL_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rpi-sub-jsonl4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A path whose parent is a FILE cannot be created → create() yields
        // a handle-less writer; write_line must be a silent no-op.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a dir").unwrap();
        let path = blocker.join("events.jsonl");
        let mut writer = JsonlWriter::create(&path, DEFAULT_MAX_JSONL_BYTES);
        writer.write_line(r#"{"type":"a"}"#);
        drop(writer);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- TE17 (R7.1.7.1): bounded write retry + error classification ----

    #[cfg(unix)]
    #[test]
    fn retryable_error_classification() {
        for code in [
            libc::ENOSPC,
            libc::EDQUOT,
            libc::EMFILE,
            libc::ENFILE,
            libc::EBUSY,
            libc::EAGAIN,
        ] {
            let error = std::io::Error::from_raw_os_error(code);
            assert!(
                is_retryable_write_error(&error),
                "errno {code} must be retryable"
            );
            assert_eq!(write_attempts_for(&error), WRITE_METADATA_MAX_ATTEMPTS);
        }
        for code in [libc::ENOENT, libc::EACCES, libc::EPERM, libc::EISDIR] {
            let error = std::io::Error::from_raw_os_error(code);
            assert!(
                !is_retryable_write_error(&error),
                "errno {code} must not be retryable"
            );
            assert_eq!(write_attempts_for(&error), 1);
        }
        // Errors without an errno code never retry.
        assert!(!is_retryable_write_error(&std::io::Error::other("boom")));
    }

    #[test]
    fn write_retry_backoff_is_bounded_and_exponential() {
        assert_eq!(write_retry_backoff_ms(1), WRITE_METADATA_BACKOFF_BASE_MS);
        assert_eq!(
            write_retry_backoff_ms(2),
            WRITE_METADATA_BACKOFF_BASE_MS * 2
        );
        assert_eq!(
            write_retry_backoff_ms(3),
            WRITE_METADATA_BACKOFF_BASE_MS * 4
        );
        assert_eq!(
            write_retry_backoff_ms(4),
            WRITE_METADATA_BACKOFF_BASE_MS * 8
        );
        let total: u64 = (1..WRITE_METADATA_MAX_ATTEMPTS as u64)
            .map(|attempt| write_retry_backoff_ms(attempt as u32))
            .sum();
        assert!(total <= 1_000, "whole ladder sleeps at most 1s: {total}ms");
    }

    #[cfg(unix)]
    #[test]
    fn write_metadata_retries_then_succeeds() {
        // First attempt hits a synthetic ENOSPC, later attempts write for
        // real: the caller sees Ok and the file holds the full body.
        let dir = std::env::temp_dir().join(format!("rpi-sub-retry-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.json");
        let attempts = std::cell::Cell::new(0u32);
        let result = write_metadata_retrying(&path, "{\"a\":1}\n", |tmp, target, body| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
            }
            std::fs::write(tmp, body).and_then(|()| std::fs::rename(tmp, target))
        });
        assert!(result.is_ok());
        assert_eq!(attempts.get(), 2, "one failure then success");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_metadata_exhausts_retry_ladder() {
        // A permanently-full disk: every attempt fails with ENOSPC, the
        // ladder runs out after WRITE_METADATA_MAX_ATTEMPTS tries and the
        // final error propagates.
        let path = Path::new("/tmp/definitely-not-used-here.json");
        let attempts = std::cell::Cell::new(0u32);
        let started = std::time::Instant::now();
        let result = write_metadata_retrying(path, "x", |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
        });
        assert_eq!(attempts.get(), WRITE_METADATA_MAX_ATTEMPTS);
        let error = result.expect_err("ladder exhausted");
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(write_attempts_for(&error), WRITE_METADATA_MAX_ATTEMPTS);
        // Bounded: 20+40+80+160 = 300ms of sleep plus slack.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "total retry budget stays bounded: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn write_metadata_does_not_retry_terminal_errors() {
        // EACCES fails the write immediately: one attempt, no backoff.
        let attempts = std::cell::Cell::new(0u32);
        let started = std::time::Instant::now();
        let result = write_metadata_retrying(Path::new("/nowhere/meta.json"), "x", |_, _, _| {
            attempts.set(attempts.get() + 1);
            Err(std::io::Error::from_raw_os_error(libc::EACCES))
        });
        assert_eq!(attempts.get(), 1, "permission errors never retry");
        let error = result.expect_err("EACCES propagates");
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod recursive_cleanup_tests {
    use super::*;

    fn set_age(path: &Path, days: u64) {
        let when =
            std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 60 * 60);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(when).expect("set mtime");
    }

    #[test]
    fn nested_outputs_are_cleaned_and_handoffs_patches_kept() {
        let root = std::env::temp_dir().join(format!(
            "rpi-artifacts-rec-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let outputs = root.join("outputs");
        let handoffs = root.join("handoffs");
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::create_dir_all(&handoffs).unwrap();
        let old_output = outputs.join("old.md");
        let keep_patch = root.join("recovery.patch");
        let keep_handoff = handoffs.join("run.json");
        for path in [&old_output, &keep_patch, &keep_handoff] {
            std::fs::write(path, "x").unwrap();
            set_age(path, 40);
        }
        // No throttle marker: the first sweep runs.
        cleanup_old_artifacts(&root, 7);
        assert!(!old_output.exists(), "nested output removed");
        assert!(keep_patch.exists(), "worktree patch preserved");
        assert!(keep_handoff.exists(), "handoff record preserved");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let root = std::env::temp_dir().join(format!(
            "rpi-artifacts-sym-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let outside =
            std::env::temp_dir().join(format!("rpi-artifacts-outside-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("keep.txt");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, root.join("link.txt")).unwrap();
        cleanup_old_artifacts(&root, 7);
        assert!(target.exists(), "symlink target untouched");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
