//! MCP server log messages (port of
//! `packages/coding-agent/src/extensions/mcp/log.ts` @ a13d35a74).
//!
//! `notifications/message` lines are appended to `mcp.log` in the agent
//! directory; several processes may write to the same file, so every
//! message is one append. The file rotates to `mcp.log.1` past
//! [`MAX_LOG_BYTES`]. Secrets are never logged (G4 red line).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

/// `MAX_LOG_BYTES` (log.ts:9).
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn format_data(data: &Value) -> String {
    match data {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// `formatMcpLogMessage` (log.ts:24): one `notifications/message` from
/// `server` as a log line; continuation lines are indented.
pub fn format_mcp_log_message(server: &str, params: &Value, now: std::time::SystemTime) -> String {
    let message = params.as_object().cloned().unwrap_or_else(|| {
        let mut map = serde_json::Map::new();
        map.insert("data".to_owned(), params.clone());
        map
    });
    let level = message
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let logger = match message.get("logger").and_then(Value::as_str) {
        Some(logger) if !logger.is_empty() => format!(" {logger}:"),
        _ => String::new(),
    };
    let text = format_data(message.get("data").unwrap_or(&Value::Null))
        .replace("\r\n", "\n")
        .replace('\n', "\n    ");
    let timestamp = chrono::DateTime::<chrono::Utc>::from(now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    format!("{timestamp} [{server}] {level}{logger} {text}\n")
}

/// `McpServerLog` (log.ts:35): appends server log messages to one file.
/// Write errors are ignored: logging must not break tools.
pub struct McpServerLog {
    path: PathBuf,
    size: Mutex<Option<u64>>,
}

impl McpServerLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            size: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `write` (log.ts:40).
    pub fn write(&self, server: &str, params: &Value) {
        let line = format_mcp_log_message(server, params, std::time::SystemTime::now());
        let mut size = self.size.lock().unwrap_or_else(|error| error.into_inner());
        if size.is_none() {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            *size = Some(self.current_size());
        }
        if size.unwrap_or_default() > MAX_LOG_BYTES {
            if self.current_size() > MAX_LOG_BYTES {
                let _ = std::fs::rename(&self.path, format!("{}.1", self.path.display()));
            }
            *size = Some(self.current_size());
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
            *size = Some(size.unwrap_or_default() + line.len() as u64);
        }
    }

    fn current_size(&self) -> u64 {
        std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn formats_level_logger_and_indented_lines() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000);
        let line = format_mcp_log_message(
            "docs",
            &json!({"level": "error", "logger": "srv", "data": "a\nb"}),
            now,
        );
        assert!(line.contains("[docs] error srv: a\n    b"), "{line}");
        let line = format_mcp_log_message("docs", &json!({"data": {"a": 1}}), now);
        assert!(line.contains("[docs] info {\"a\":1}"), "{line}");
    }

    #[test]
    fn rotates_past_the_limit() {
        let dir = std::env::temp_dir().join(format!("rpi-mcp-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.log");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        let log = McpServerLog::new(&path);
        log.write("s", &json!({"data": "line"}));
        assert!(dir.join("mcp.log.1").exists());
        assert!(std::fs::read_to_string(&path).unwrap().contains("line"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
