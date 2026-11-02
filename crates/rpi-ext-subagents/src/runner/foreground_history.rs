//! Foreground-run memory across host restarts (#3025 / FR-D R1).
//!
//! Upstream persists every foreground run to `results/foreground-history/`
//! so a run cut off by a Pi exit can be inspected and resumed. rpi's
//! foreground runs are synchronous child processes; their child session
//! files survive the parent process, so the rpi shape is a small per-run
//! record:
//!
//! ```text
//! <tempRoot>/async-subagent-results/foreground-history/<runId>.json
//! ```
//!
//! A launch-time record has `children[].status == "running"` and the owning
//! host pid/boot id. At the next `session_start` an interrupted record whose
//! owner is dead is rewritten as `stopped` with the discovered child session
//! file, which `subagent({action:"resume", id})` can revive. Final saves
//! replace the running record; a launch that fails before spawning discards
//! it. Composite `parallel`/`chain` runs are admitted with one child entry
//! per index (rpi extension, TE-D54; upstream reads the same record shape
//! but only writes single-run records in v0.83.0).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::runner::background::{async_results_dir, process_boot_id};

pub const MAX_REMEMBERED_FOREGROUND_RUNS: usize = 50;
pub const FOREGROUND_OWNER_EXITED_STOP_ERROR: &str =
    "Subagent stopped because the rpi process that ran it exited before the run finished.";
const MAX_INLINE_OUTPUT_BYTES: usize = 64 * 1024;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn history_dir() -> PathBuf {
    async_results_dir().join("foreground-history")
}

fn record_path(run_id: &str) -> Option<PathBuf> {
    crate::paths::ensure_safe_component(run_id, "Run id").ok()?;
    Some(history_dir().join(format!("{run_id}.json")))
}

/// UTF-8-safe head bound (`boundedTail` upstream keeps the tail; the rpi
/// record keeps the head so a resume preview starts at the report title).
fn bounded_output(value: &str) -> String {
    if value.len() <= MAX_INLINE_OUTPUT_BYTES {
        return value.to_string();
    }
    let mut end = MAX_INLINE_OUTPUT_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Newest `.jsonl` session file under `dir`, if any (the child's session
/// filename is host-assigned, so the launch record only knows the directory).
pub(crate) fn newest_session_file(dir: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, best: &mut Option<(SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                walk(&path, best);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            if best.as_ref().is_none_or(|(current, _)| modified > *current) {
                *best = Some((modified, path));
            }
        }
    }
    let mut best = None;
    walk(dir, &mut best);
    best.map(|(_, path)| path)
}

fn write_record(run_id: &str, record: &Value) -> Result<(), String> {
    let path = record_path(run_id).ok_or_else(|| "invalid run id".to_string())?;
    let dir = path.parent().unwrap_or(Path::new("."));
    crate::paths::create_private_dir_all(dir).map_err(|error| error.to_string())?;
    crate::artifacts::write_metadata(&path, record).map_err(|error| error.to_string())?;
    // Final records inline up to 64 KiB of child output; keep the file
    // owner-only (`writePrivateAtomicJson` upstream).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn read_record(path: &Path) -> Option<Value> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read foreground run history");
            return None;
        }
    };
    let record: Value = match serde_json::from_str(&raw) {
        Ok(record) => record,
        Err(error) => {
            // FR-D R2: a corrupt record is skipped, but never silently.
            tracing::warn!(path = %path.display(), %error, "skipping corrupt foreground run history");
            return None;
        }
    };
    if record["runId"].as_str().is_none()
        || record["children"]
            .as_array()
            .is_none_or(|children| children.is_empty())
    {
        tracing::warn!(path = %path.display(), "skipping malformed foreground run history");
        return None;
    }
    Some(record)
}

/// Metadata shared by the launch-time and final record writes.
pub struct ForegroundRunMeta<'a> {
    pub run_id: &'a str,
    pub session_id: Option<&'a str>,
    pub cwd: &'a Path,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub thinking: Option<&'a str>,
    pub child_cwd: &'a Path,
    /// Per-run session root (unique) or `None` for an explicit shared root.
    pub session_dir: Option<&'a Path>,
    /// Exact resume/continuation file when known (explicit `sessionFile`).
    pub session_file: Option<&'a Path>,
}

/// One child in a launch-time record.
pub struct ForegroundRunningChild<'a> {
    pub index: usize,
    pub agent: &'a str,
    pub child_cwd: &'a Path,
    pub model: Option<&'a str>,
    pub thinking: Option<&'a str>,
    /// Per-run session root (unique) or `None` for an explicit shared root.
    pub session_dir: Option<&'a Path>,
    /// Exact resume/continuation file when known (explicit `sessionFile`).
    pub session_file: Option<&'a Path>,
}

/// One child in a final record.
pub struct ForegroundFinalChild<'a> {
    pub index: usize,
    pub agent: &'a str,
    pub child_cwd: &'a Path,
    pub model: Option<&'a str>,
    pub thinking: Option<&'a str>,
    pub session_dir: Option<&'a Path>,
    pub result: &'a crate::runner::foreground::ForegroundRunResult,
}

/// Launch-time record for one single foreground child.
pub fn persist_running(meta: &ForegroundRunMeta<'_>) {
    persist_running_children(
        meta.run_id,
        meta.session_id,
        meta.cwd,
        "single",
        &[ForegroundRunningChild {
            index: 0,
            agent: meta.agent,
            child_cwd: meta.child_cwd,
            model: meta.model,
            thinking: meta.thinking,
            session_dir: meta.session_dir,
            session_file: meta.session_file,
        }],
    );
}

/// Launch-time record for a composite (`parallel` / `chain`) foreground run.
/// Upstream's run history admits `single`/`parallel`/`chain` records
/// (foreground-history.ts:135-152); rpi keeps the same per-child shape so a
/// host exit before the batch finishes leaves every child resumable.
pub fn persist_running_children(
    run_id: &str,
    session_id: Option<&str>,
    cwd: &Path,
    mode: &str,
    children: &[ForegroundRunningChild<'_>],
) {
    // Upstream `compactRun` refuses records without a session id; a record
    // that no session can resolve is worse than no record.
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return;
    };
    if children.is_empty() {
        return;
    }
    let children: Vec<Value> = children
        .iter()
        .map(|child| {
            let mut value = json!({
                "agent": child.agent,
                "index": child.index,
                "status": "running",
                "cwd": child.child_cwd.to_string_lossy(),
            });
            if let Some(dir) = child.session_dir {
                value["sessionDir"] = json!(dir.to_string_lossy());
            }
            if let Some(file) = child.session_file {
                value["sessionFile"] = json!(file.to_string_lossy());
            }
            if let Some(model) = child.model {
                value["model"] = json!(model);
            }
            if let Some(thinking) = child.thinking {
                value["thinking"] = json!(thinking);
            }
            value
        })
        .collect();
    let record = json!({
        "runId": run_id,
        "mode": mode,
        "cwd": cwd.to_string_lossy(),
        "sessionId": session_id,
        "updatedAt": now_millis(),
        "owner": {
            "pid": std::process::id(),
            "bootId": process_boot_id(std::process::id() as u64),
        },
        "children": children,
    });
    if let Err(error) = write_record(run_id, &record) {
        tracing::warn!(run = %run_id, %error, "failed to persist foreground run history");
    }
}

/// Final record replacing the launch-time running record.
pub fn persist_final(
    meta: &ForegroundRunMeta<'_>,
    result: &crate::runner::foreground::ForegroundRunResult,
) {
    persist_final_children(
        meta.run_id,
        meta.session_id,
        meta.cwd,
        "single",
        &[ForegroundFinalChild {
            index: 0,
            agent: meta.agent,
            child_cwd: meta.child_cwd,
            model: meta.model,
            thinking: meta.thinking,
            session_dir: meta.session_dir,
            result,
        }],
    );
}

/// Final record for a composite (`parallel` / `chain`) foreground run.
pub fn persist_final_children(
    run_id: &str,
    session_id: Option<&str>,
    cwd: &Path,
    mode: &str,
    children: &[ForegroundFinalChild<'_>],
) {
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return;
    };
    if children.is_empty() {
        return;
    }
    let values: Vec<Value> = children.iter().map(final_child_json).collect();
    let record = json!({
        "runId": run_id,
        "mode": mode,
        "cwd": cwd.to_string_lossy(),
        "sessionId": session_id,
        "updatedAt": now_millis(),
        "children": values,
    });
    if let Err(error) = write_record(run_id, &record) {
        tracing::warn!(run = %run_id, %error, "failed to persist foreground run history");
    }
    prune();
}

fn final_child_json(child: &ForegroundFinalChild<'_>) -> Value {
    let result = child.result;
    let status = if result.exit_code == 0
        && result.error.is_none()
        && !result.timed_out
        && result.process_signal.is_none()
    {
        "completed"
    } else {
        "failed"
    };
    let mut value = json!({
        "agent": child.agent,
        "index": child.index,
        "status": status,
        "cwd": child.child_cwd.to_string_lossy(),
        "exitCode": result.exit_code,
        "timedOut": result.timed_out,
        "finalOutput": bounded_output(&result.final_output),
    });
    if let Some(model) = result.model.as_deref().or(child.model) {
        value["model"] = json!(model);
    }
    if let Some(thinking) = result.thinking.as_deref().or(child.thinking) {
        value["thinking"] = json!(thinking);
    }
    if let Some(error) = &result.error {
        value["error"] = json!(error);
    }
    if let Some(signal) = &result.process_signal {
        value["processSignal"] = json!(signal);
    }
    // Keep the derived per-run root on the final record too: a later
    // `restore`-style fallback can find the child session even if the host
    // changed its filename, and `status` keeps the launch shape.
    if let Some(dir) = child.session_dir {
        value["sessionDir"] = json!(dir.to_string_lossy());
    }
    if let Some(path) = &result.session_file {
        value["sessionFile"] = json!(path.to_string_lossy());
    }
    if let Some(paths) = &result.artifact_paths {
        value["artifactPaths"] = paths.to_json();
    }
    if result.output_partial {
        value["outputPartial"] = json!(true);
    }
    value
}

/// Drop a running record for a launch that never spawned a child.
pub fn discard_running(run_id: &str) {
    let Some(path) = record_path(run_id) else {
        return;
    };
    let Some(record) = read_record(&path) else {
        return;
    };
    let running = record["children"]
        .as_array()
        .is_some_and(|children| children.iter().any(|child| child["status"] == "running"));
    let own_record = record["owner"]["pid"].as_u64() == Some(std::process::id() as u64);
    if running && own_record {
        let _ = std::fs::remove_file(path);
    }
}

/// Rewrite interrupted runs whose owner host is dead. Returns the number
/// of records restored.
pub fn restore_interrupted() -> usize {
    let Ok(entries) = std::fs::read_dir(history_dir()) else {
        return 0;
    };
    let mut restored = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(mut record) = read_record(&path) else {
            continue;
        };
        let Some(run_id) = record["runId"].as_str().map(str::to_string) else {
            continue;
        };
        let owner = &record["owner"];
        let owner_alive = owner["pid"]
            .as_u64()
            .is_some_and(|pid| owner_may_be_alive(pid, owner["bootId"].as_u64()));
        if owner_alive {
            continue;
        }
        let Some(children) = record["children"].as_array_mut() else {
            continue;
        };
        let mut changed = false;
        for child in children.iter_mut() {
            if child["status"] != "running" {
                continue;
            }
            let session_file = child["sessionFile"]
                .as_str()
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .or_else(|| {
                    child["sessionDir"]
                        .as_str()
                        .map(PathBuf::from)
                        .and_then(|dir| newest_session_file(&dir))
                });
            child["status"] = json!("stopped");
            child["exitCode"] = json!(1);
            child["error"] = json!(FOREGROUND_OWNER_EXITED_STOP_ERROR);
            child["finalOutput"] = json!(FOREGROUND_OWNER_EXITED_STOP_ERROR);
            if let Some(session_file) = session_file {
                child["sessionFile"] = json!(session_file.to_string_lossy());
            }
            changed = true;
        }
        if changed {
            record["updatedAt"] = json!(now_millis());
            if let Some(object) = record.as_object_mut() {
                object.remove("owner");
            }
            if write_record(&run_id, &record).is_ok() {
                // A restored run is terminal; its pending supervisor asks
                // must not keep a later wait/drain yielding.
                crate::p1::supervisor::close_requests_for_run(&run_id);
                restored += 1;
            }
        }
    }
    prune();
    restored
}

/// Whether a recorded owning process may still be alive. A missing boot id
/// degrades to a plain liveness check; another boot is dead without probing.
#[cfg(unix)]
fn owner_may_be_alive(pid: u64, recorded_boot_id: Option<u64>) -> bool {
    if pid == 0 {
        return false;
    }
    crate::runner::background::recorded_pid_matches(pid, recorded_boot_id)
}

/// Non-unix fallback: rpi has no process-start-time probe there, and the
/// capacity-slot path deliberately treats every pid as alive. For foreground
/// history that would disable restore entirely, so only the current process
/// is assumed alive; another host's record is treated as interrupted. This
/// is conservative for recovery (a still-running foreign child could be
/// resumed twice) and is registered as a Windows variant, not parity.
#[cfg(not(unix))]
fn owner_may_be_alive(pid: u64, _recorded_boot_id: Option<u64>) -> bool {
    pid == std::process::id() as u64
}

/// One remembered record by exact run id.
pub fn find(run_id: &str) -> Option<Value> {
    let path = record_path(run_id)?;
    read_record(&path)
}

fn matches_session(record: &Value, session_id: Option<&str>) -> bool {
    match session_id.filter(|id| !id.is_empty()) {
        Some(session_id) => record["sessionId"].as_str() == Some(session_id),
        None => false,
    }
}

/// Resolve an exact id or a unique prefix among remembered records, optionally
/// restricted to the caller's session (upstream `resolveForegroundResumeTarget`
/// only resolves runs from the current session).
pub fn find_prefix_for_session(
    requested: &str,
    session_id: Option<&str>,
) -> Result<Option<Value>, String> {
    let requested = requested.trim();
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    if requested.is_empty() {
        return Ok(None);
    }
    if let Some(record) = find(requested).filter(|record| matches_session(record, Some(session_id)))
    {
        return Ok(Some(record));
    }
    let Ok(entries) = std::fs::read_dir(history_dir()) else {
        return Ok(None);
    };
    let mut matches: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| read_record(&entry.path()))
        .filter(|record| matches_session(record, Some(session_id)))
        .filter(|record| {
            record["runId"]
                .as_str()
                .is_some_and(|run_id| run_id.starts_with(requested))
        })
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => {
            let mut ids: Vec<String> = matches
                .iter()
                .filter_map(|record| record["runId"].as_str().map(str::to_string))
                .collect();
            ids.sort();
            Err(format!(
                "Ambiguous foreground run id prefix '{requested}' matched: {}. Provide a longer id.",
                ids.join(", ")
            ))
        }
    }
}

/// Remembered records for one parent session, newest first.
pub fn list_for_session(session_id: Option<&str>, limit: usize) -> Vec<Value> {
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(history_dir()) else {
        return Vec::new();
    };
    let mut records: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| read_record(&entry.path()))
        .filter(|record| record["sessionId"].as_str() == Some(session_id))
        .collect();
    records.sort_by(|left, right| {
        right["updatedAt"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&left["updatedAt"].as_u64().unwrap_or(0))
    });
    records.truncate(limit);
    records
}

/// Delete old final records beyond the cap. Running records are skipped
/// (another live host replaces them later); an interrupted record is final.
fn prune() {
    let Ok(entries) = std::fs::read_dir(history_dir()) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= MAX_REMEMBERED_FOREGROUND_RUNS {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let remove_count = files.len() - MAX_REMEMBERED_FOREGROUND_RUNS;
    for (modified, path) in files.into_iter().take(remove_count) {
        let running = read_record(&path)
            .and_then(|record| record["children"].as_array().cloned())
            .is_some_and(|children| children.iter().any(|child| child["status"] == "running"));
        if running {
            continue;
        }
        let still_same = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|current| current == modified);
        if still_same {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Select the child a resume revives (`resolveForegroundResumeTarget`,
/// subagent-executor.ts:987-1027): a multi-child (`parallel`/`chain`) run
/// requires an explicit index; the single-child run defaults to index 0.
/// Every failure names the offending child and file.
pub fn child_for_resume(record: &Value, index: Option<usize>) -> Result<&Value, String> {
    let run_id = record["runId"].as_str().unwrap_or("?");
    let Some(children) = record["children"].as_array() else {
        return Err(format!(
            "Foreground run '{run_id}' does not have a persisted child session to resume from."
        ));
    };
    // Upstream refuses to revive a run with any detached child while it may
    // still be live (subagent-executor.ts:997).
    if children
        .iter()
        .any(|child| child["status"].as_str() == Some("detached"))
    {
        return Err(format!(
            "Foreground run '{run_id}' is detached for intercom coordination and cannot be revived safely while any child may still be live. Reply to the supervisor request first, then wait with bg_wait({{ id: \"{run_id}\" }}); use status to recover the result and do not launch a replacement while it remains detached."
        ));
    }
    if children.len() > 1 && index.is_none() {
        return Err(format!(
            "Foreground run '{run_id}' has {} children. Provide index to choose one.",
            children.len()
        ));
    }
    let index = index.unwrap_or(0);
    if index >= children.len() {
        return Err(format!(
            "Foreground run '{run_id}' has {} children. Index {index} is out of range.",
            children.len()
        ));
    }
    let child = &children[index];
    let Some(path) = child["sessionFile"]
        .as_str()
        .filter(|path| !path.is_empty())
    else {
        return Err(format!(
            "Foreground run '{run_id}' child {index} does not have a persisted session file to resume from."
        ));
    };
    if !path.ends_with(".jsonl") {
        return Err(format!(
            "Foreground run '{run_id}' child {index} session file must be a .jsonl file: {path}"
        ));
    }
    if !Path::new(path).is_file() {
        return Err(format!(
            "Foreground run '{run_id}' child {index} session file does not exist: {path}"
        ));
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `async_results_dir()` is process-global; history tests must not see
    /// another test's running record during `restore_interrupted`.
    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-fg-history-{tag}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn interrupted_owner_restores_with_the_child_session_file() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let dir = temp_root("restore");
        let run_id = format!("fg-{}", now_millis());
        let session_file = dir.join("run-0").join("session.jsonl");
        std::fs::create_dir_all(session_file.parent().unwrap()).unwrap();
        std::fs::write(&session_file, "{}\n").unwrap();
        let record = json!({
            "runId": run_id,
            "mode": "single",
            "cwd": dir.to_string_lossy(),
            "sessionId": "session-1",
            "updatedAt": 1,
            "owner": { "pid": 4_000_000, "bootId": Value::Null },
            "children": [{
                "agent": "worker",
                "index": 0,
                "status": "running",
                "sessionDir": dir.to_string_lossy(),
            }],
        });
        write_record(&run_id, &record).unwrap();
        restore_interrupted();
        let restored = find(&run_id).expect("record present");
        assert_eq!(restored["children"][0]["status"], "stopped");
        assert_eq!(
            restored["children"][0]["sessionFile"].as_str(),
            Some(session_file.to_str().unwrap())
        );
        assert!(restored.get("owner").is_none());
        // The restore is idempotent for this record.
        restore_interrupted();
        assert_eq!(find(&run_id).unwrap()["children"][0]["status"], "stopped");
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(path) = record_path(&run_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn live_owner_is_not_restored() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let run_id = format!("fg-live-{}", now_millis());
        let record = json!({
            "runId": run_id,
            "mode": "single",
            "cwd": "/tmp",
            "sessionId": "session-live",
            "updatedAt": 1,
            "owner": {
                "pid": std::process::id(),
                "bootId": process_boot_id(std::process::id() as u64),
            },
            "children": [{ "agent": "worker", "index": 0, "status": "running" }],
        });
        write_record(&run_id, &record).unwrap();
        restore_interrupted();
        assert_eq!(find(&run_id).unwrap()["children"][0]["status"], "running");
        let _ = record_path(&run_id).map(std::fs::remove_file);
    }

    #[test]
    fn resume_requires_a_persisted_session_file() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let record = json!({
            "runId": "fg-none",
            "children": [{ "agent": "worker", "index": 0, "status": "stopped" }],
        });
        assert!(child_for_resume(&record, None).is_err());
    }

    #[test]
    fn composite_resume_requires_an_index_and_selects_that_child() {
        let dir = temp_root("composite-resume");
        let first = dir.join("first.jsonl");
        let second = dir.join("second.jsonl");
        std::fs::write(&first, "{}").unwrap();
        std::fs::write(&second, "{}").unwrap();
        let record = json!({
            "runId": "fg-composite",
            "mode": "parallel",
            "cwd": "/tmp",
            "sessionId": "session-a",
            "children": [
                { "agent": "scout", "index": 0, "status": "completed",
                  "sessionFile": first.to_string_lossy() },
                { "agent": "worker", "index": 1, "status": "failed",
                  "sessionFile": second.to_string_lossy() },
            ],
        });
        let error = child_for_resume(&record, None).unwrap_err();
        assert!(
            error.contains("has 2 children. Provide index to choose one."),
            "{error}"
        );
        assert_eq!(
            child_for_resume(&record, Some(1)).unwrap()["agent"],
            json!("worker")
        );
        let error = child_for_resume(&record, Some(2)).unwrap_err();
        assert!(error.contains("Index 2 is out of range"), "{error}");
        let missing = json!({
            "runId": "fg-composite",
            "mode": "chain",
            "children": [
                { "agent": "scout", "index": 0, "status": "completed" },
                { "agent": "worker", "index": 1, "status": "failed" },
            ],
        });
        let error = child_for_resume(&missing, Some(1)).unwrap_err();
        assert!(
            error.contains("child 1 does not have a persisted session file"),
            "{error}"
        );
    }

    #[test]
    fn composite_history_round_trips_children_and_session_files() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let dir = temp_root("composite-history");
        let first = dir.join("first.jsonl");
        let second = dir.join("second.jsonl");
        std::fs::write(&first, "{}").unwrap();
        std::fs::write(&second, "{}").unwrap();
        let run_id = format!("fg-composite-history-{}", now_millis());
        let child_path = dir.join("child");
        let ok = crate::runner::foreground::ForegroundRunResult {
            exit_code: 0,
            final_output: "first output".to_string(),
            model: Some("faux/1".to_string()),
            session_file: Some(first.clone()),
            ..Default::default()
        };
        let failed = crate::runner::foreground::ForegroundRunResult {
            exit_code: 1,
            error: Some("boom".to_string()),
            final_output: "second output".to_string(),
            session_file: Some(second.clone()),
            ..Default::default()
        };
        let running = [
            ForegroundRunningChild {
                index: 0,
                agent: "scout",
                child_cwd: &child_path,
                model: None,
                thinking: None,
                session_dir: Some(&dir),
                session_file: None,
            },
            ForegroundRunningChild {
                index: 1,
                agent: "worker",
                child_cwd: &child_path,
                model: None,
                thinking: None,
                session_dir: Some(&dir),
                session_file: None,
            },
        ];
        persist_running_children(&run_id, Some("session-a"), &dir, "parallel", &running);
        let record = find(&run_id).expect("running record");
        assert_eq!(record["mode"], json!("parallel"));
        assert_eq!(record["children"].as_array().unwrap().len(), 2);
        assert_eq!(record["children"][1]["status"], json!("running"));

        let finals = [
            ForegroundFinalChild {
                index: 0,
                agent: "scout",
                child_cwd: &child_path,
                model: None,
                thinking: None,
                session_dir: Some(&dir),
                result: &ok,
            },
            ForegroundFinalChild {
                index: 1,
                agent: "worker",
                child_cwd: &child_path,
                model: None,
                thinking: None,
                session_dir: Some(&dir),
                result: &failed,
            },
        ];
        persist_final_children(&run_id, Some("session-a"), &dir, "parallel", &finals);
        let record = find(&run_id).expect("final record");
        assert_eq!(record["children"][0]["status"], json!("completed"));
        assert_eq!(record["children"][1]["status"], json!("failed"));
        assert_eq!(
            record["children"][1]["sessionFile"],
            json!(second.to_string_lossy())
        );
        assert!(
            child_for_resume(&record, Some(1)).is_ok(),
            "final composite record must resolve its failed child"
        );
        assert_eq!(
            list_for_session(Some("session-a"), 10)[0]["runId"],
            json!(run_id)
        );
        let _ = record_path(&run_id).map(std::fs::remove_file);
    }

    #[test]
    fn resume_resolution_is_scoped_to_the_current_session() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let run_id = format!("fg-scope-{}", now_millis());
        let record = json!({
            "runId": run_id,
            "mode": "single",
            "cwd": "/tmp",
            "sessionId": "session-a",
            "updatedAt": 1,
            "children": [{ "agent": "worker", "index": 0, "status": "completed" }],
        });
        write_record(&run_id, &record).unwrap();
        assert!(
            find_prefix_for_session(&run_id, Some("session-a"))
                .unwrap()
                .is_some()
        );
        assert!(
            find_prefix_for_session(&run_id, Some("session-b"))
                .unwrap()
                .is_none()
        );
        assert!(find_prefix_for_session(&run_id, None).unwrap().is_none());
        assert!(
            find_prefix_for_session(&run_id, Some(""))
                .unwrap()
                .is_none()
        );
        let _ = record_path(&run_id).map(std::fs::remove_file);
    }

    #[test]
    fn resume_requires_a_jsonl_session_file() {
        let dir = temp_root("jsonl");
        let session = dir.join("session.txt");
        std::fs::write(&session, "{}").unwrap();
        let record = json!({
            "runId": "fg-text",
            "children": [{
                "agent": "worker",
                "index": 0,
                "status": "stopped",
                "sessionFile": session.to_string_lossy(),
            }],
        });
        assert!(
            child_for_resume(&record, None).is_err(),
            "non-jsonl rejected"
        );
        let jsonl = dir.join("session.jsonl");
        std::fs::write(&jsonl, "{}").unwrap();
        let record = json!({
            "runId": "fg-jsonl",
            "children": [{
                "agent": "worker",
                "index": 0,
                "status": "stopped",
                "sessionFile": jsonl.to_string_lossy(),
            }],
        });
        assert!(child_for_resume(&record, None).is_ok(), "jsonl accepted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_is_bounded_at_a_utf8_boundary() {
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let text = "é".repeat(MAX_INLINE_OUTPUT_BYTES);
        let bounded = bounded_output(&text);
        assert!(bounded.len() <= MAX_INLINE_OUTPUT_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
