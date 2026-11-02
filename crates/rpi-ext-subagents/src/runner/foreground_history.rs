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
//! it. Composite foreground runs are not admitted yet (their child session
//! files are resolved per index; the single-run case is the resumable one
//! upstream also admits at launch).

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
fn newest_session_file(dir: &Path) -> Option<PathBuf> {
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
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    crate::artifacts::write_metadata(&path, record).map_err(|error| error.to_string())
}

fn read_record(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    let record: Value = serde_json::from_str(&raw).ok()?;
    record["runId"].as_str()?;
    record["children"]
        .as_array()
        .filter(|children| !children.is_empty())?;
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
    pub session_dir: &'a Path,
}

/// Launch-time record for one single foreground child.
pub fn persist_running(meta: &ForegroundRunMeta<'_>) {
    let mut child = json!({
        "agent": meta.agent,
        "index": 0,
        "status": "running",
        "cwd": meta.child_cwd.to_string_lossy(),
        "sessionDir": meta.session_dir.to_string_lossy(),
    });
    if let Some(model) = meta.model {
        child["model"] = json!(model);
    }
    if let Some(thinking) = meta.thinking {
        child["thinking"] = json!(thinking);
    }
    let record = json!({
        "runId": meta.run_id,
        "mode": "single",
        "cwd": meta.cwd.to_string_lossy(),
        "sessionId": meta.session_id.unwrap_or_default(),
        "updatedAt": now_millis(),
        "owner": {
            "pid": std::process::id(),
            "bootId": process_boot_id(std::process::id() as u64),
        },
        "children": [child],
    });
    if let Err(error) = write_record(meta.run_id, &record) {
        tracing::warn!(run = %meta.run_id, %error, "failed to persist foreground run history");
    }
}

/// Final record replacing the launch-time running record.
pub fn persist_final(
    meta: &ForegroundRunMeta<'_>,
    result: &crate::runner::foreground::ForegroundRunResult,
) {
    let status = if result.exit_code == 0
        && result.error.is_none()
        && !result.timed_out
        && result.process_signal.is_none()
    {
        "completed"
    } else {
        "failed"
    };
    let mut child = json!({
        "agent": meta.agent,
        "index": 0,
        "status": status,
        "cwd": meta.child_cwd.to_string_lossy(),
        "exitCode": result.exit_code,
        "timedOut": result.timed_out,
        "finalOutput": bounded_output(&result.final_output),
    });
    if let Some(model) = result.model.as_deref().or(meta.model) {
        child["model"] = json!(model);
    }
    if let Some(thinking) = result.thinking.as_deref().or(meta.thinking) {
        child["thinking"] = json!(thinking);
    }
    if let Some(error) = &result.error {
        child["error"] = json!(error);
    }
    if let Some(signal) = &result.process_signal {
        child["processSignal"] = json!(signal);
    }
    if let Some(path) = &result.session_file {
        child["sessionFile"] = json!(path.to_string_lossy());
    }
    if let Some(paths) = &result.artifact_paths {
        child["artifactPaths"] = paths.to_json();
    }
    if result.output_partial {
        child["outputPartial"] = json!(true);
    }
    let record = json!({
        "runId": meta.run_id,
        "mode": "single",
        "cwd": meta.cwd.to_string_lossy(),
        "sessionId": meta.session_id.unwrap_or_default(),
        "updatedAt": now_millis(),
        "children": [child],
    });
    if let Err(error) = write_record(meta.run_id, &record) {
        tracing::warn!(run = %meta.run_id, %error, "failed to persist foreground run history");
    }
    prune();
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
    if running {
        let _ = std::fs::remove_file(path);
    }
}

/// Rewrite interrupted single runs whose owner host is dead. Returns the
/// number of records restored.
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
                restored += 1;
            }
        }
    }
    prune();
    restored
}

/// Whether a recorded owning process may still be alive. A missing boot id
/// degrades to a plain liveness check; another boot is dead without probing.
fn owner_may_be_alive(pid: u64, recorded_boot_id: Option<u64>) -> bool {
    if pid == 0 {
        return false;
    }
    crate::runner::background::recorded_pid_matches(pid, recorded_boot_id)
}

/// One remembered record by exact run id.
pub fn find(run_id: &str) -> Option<Value> {
    let path = record_path(run_id)?;
    read_record(&path)
}

/// Resolve an exact id or a unique prefix among remembered records.
pub fn find_prefix(requested: &str) -> Result<Option<Value>, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Ok(None);
    }
    if let Some(record) = find(requested) {
        return Ok(Some(record));
    }
    let Ok(entries) = std::fs::read_dir(history_dir()) else {
        return Ok(None);
    };
    let mut matches: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| read_record(&entry.path()))
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
    let Ok(entries) = std::fs::read_dir(history_dir()) else {
        return Vec::new();
    };
    let mut records: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| read_record(&entry.path()))
        .filter(|record| {
            let recorded = record["sessionId"].as_str().unwrap_or_default();
            session_id.is_none_or(|session_id| recorded.is_empty() || recorded == session_id)
        })
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

/// The resumable child of a remembered record: the last child with a
/// persisted session file. `None` when there is nothing to revive.
pub fn resumable_child(record: &Value) -> Option<&Value> {
    let children = record["children"].as_array()?;
    children.iter().rev().find(|child| {
        child["sessionFile"]
            .as_str()
            .is_some_and(|path| !path.is_empty() && Path::new(path).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(restore_interrupted(), 1);
        let restored = find(&run_id).expect("record present");
        assert_eq!(restored["children"][0]["status"], "stopped");
        assert_eq!(
            restored["children"][0]["sessionFile"].as_str(),
            Some(session_file.to_str().unwrap())
        );
        assert!(restored.get("owner").is_none());
        // The restore is idempotent.
        assert_eq!(restore_interrupted(), 0);
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(path) = record_path(&run_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn live_owner_is_not_restored() {
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
        assert_eq!(restore_interrupted(), 0);
        assert_eq!(find(&run_id).unwrap()["children"][0]["status"], "running");
        let _ = record_path(&run_id).map(std::fs::remove_file);
    }

    #[test]
    fn resume_requires_a_persisted_session_file() {
        let record = json!({
            "runId": "fg-none",
            "children": [{ "agent": "worker", "index": 0, "status": "stopped" }],
        });
        assert!(resumable_child(&record).is_none());
    }

    #[test]
    fn output_is_bounded_at_a_utf8_boundary() {
        let text = "é".repeat(MAX_INLINE_OUTPUT_BYTES);
        let bounded = bounded_output(&text);
        assert!(bounded.len() <= MAX_INLINE_OUTPUT_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
