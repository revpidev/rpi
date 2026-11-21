//! Schedule record validation (V19-03 S21; upstream `src/schedules/*`).
//!
//! rpi has no schedule runner/consumer, but the record parser is shared by
//! the management/status diagnostics. A paused interval record without a
//! `nextRunAt` is valid (it has nothing to run), and an unreadable record is
//! reported once per session instead of once per scan.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleRecord {
    pub id: String,
    pub paused: bool,
    pub next_run_at: Option<String>,
    pub interval: Option<String>,
}

static WARNED: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Parse one schedule record. A paused record with no `nextRunAt` is valid.
pub fn parse_record(value: &Value) -> Result<ScheduleRecord, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "schedule record must be an object".to_string())?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "schedule record requires a non-empty id".to_string())?;
    let paused = object
        .get("paused")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let interval = object
        .get("interval")
        .and_then(Value::as_str)
        .filter(|interval| !interval.trim().is_empty())
        .map(str::to_string);
    let next_run_at = object
        .get("nextRunAt")
        .and_then(Value::as_str)
        .filter(|next| !next.trim().is_empty())
        .map(str::to_string);
    if interval.is_none() && next_run_at.is_none() && !paused {
        return Err("active schedule record requires interval or nextRunAt".to_string());
    }
    Ok(ScheduleRecord {
        id: id.to_string(),
        paused,
        next_run_at,
        interval,
    })
}

/// Read and parse one record. `Ok(None)` = absent; `Err` = unreadable.
pub fn read_record(path: &Path) -> Result<Option<ScheduleRecord>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read '{}': {error}", path.display())),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid schedule record '{}': {error}", path.display()))?;
    parse_record(&value).map(Some)
}

/// True once per `(session, path)` pair; the caller surfaces the warning.
pub fn warn_once(session_id: &str, path: &Path) -> bool {
    WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(format!("{session_id}\0{}", path.display()))
}

pub fn reset_warnings_for_test() {
    WARNED.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paused_interval_without_next_run_is_valid() {
        let record = parse_record(&json!({
            "id": "s1",
            "paused": true,
            "interval": "1h"
        }))
        .expect("paused record");
        assert!(record.paused);
        assert_eq!(record.next_run_at, None);
        assert_eq!(record.interval.as_deref(), Some("1h"));
    }

    #[test]
    fn active_record_needs_a_next_run() {
        assert!(parse_record(&json!({"id": "s1", "paused": false})).is_err());
        assert!(parse_record(&json!({"id": "s1", "nextRunAt": "2026-01-01T00:00:00Z"})).is_ok());
    }

    #[test]
    fn unreadable_record_warns_once_per_session() {
        reset_warnings_for_test();
        let path = Path::new("/tmp/missing-schedule.json");
        assert!(warn_once("sess-a", path));
        assert!(!warn_once("sess-a", path));
        assert!(warn_once("sess-b", path));
    }
}
