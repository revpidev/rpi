//! The `statusLine` settings key (Claude Code-compatible) read from the
//! global settings.json (TE12 FR-A).
//!
//! The plugin reads the same file the host reads, read-only — the
//! subagents `read_settings_pair` precedent (the ABI has no settings
//! accessor). Scope: global only (user decision 2026-08-19). The host's
//! settings writer uses flock + atomic temp+rename, so an unlocked
//! `read_to_string` either sees the old or the new inode in full — no
//! tearing, no WouldBlock (TE12 verification).
//!
//! The `statusLine` key doubles as the feature switch: missing key, broken
//! JSON, `type != "command"`, or an empty `command` all yield `None` and
//! the plugin never touches the footer (extensions load on directory
//! presence — there is no per-plugin toggle).

use std::path::Path;

use serde_json::Value;

use crate::paths::get_agent_dir;

/// Top-level settings key (CC-compatible spelling).
pub const STATUSLINE_KEY: &str = "statusLine";

/// `timeoutMs` bounds (rpi extension; CC documents no timeout).
pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
pub const MIN_TIMEOUT_MS: u64 = 500;
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// `padding` bound (defensive; CC documents no bound).
pub const MAX_PADDING: usize = 16;

/// Where the script output renders (rpi extension; user decision
/// 2026-08-19 — CC renders an independent row above its footer, a position
/// rpi has no channel for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// `ui.setFooter` — replace the built-in footer entirely (multi-line +
    /// full ANSI; the built-in footer's rows — including other plugins'
    /// status entries — are hidden while active).
    Replace,
    /// `ui.setStatus` — append below the built-in footer (single line: the
    /// host joins all extension statuses onto one row and folds
    /// multi-line whitespace).
    Status,
    /// `ui.setWidget` below the editor — a full ComponentTree (same
    /// multi-line + ANSI rendering as Replace) shown between the editor
    /// and the built-in footer, which stays intact (the CC "independent
    /// row above the footer" position). Other plugins' status entries
    /// keep rendering.
    Widget,
}

impl Placement {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("status") => Placement::Status,
            Some("widget") => Placement::Widget,
            _ => Placement::Replace,
        }
    }
}

/// Parsed `statusLine` object. Unknown sub-keys are ignored (the file is
/// free-form JSON and the host round-trips unknown keys verbatim).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusLineConfig {
    pub command: String,
    /// Leading spaces before each output line (CC `padding`, default 0).
    pub padding: usize,
    /// Periodic re-run in seconds (CC `refreshInterval`, min 1; `None` =
    /// event-driven only).
    pub refresh_interval_secs: Option<u64>,
    pub placement: Placement,
    /// Script execution timeout (rpi `timeoutMs` extension).
    pub timeout_ms: u64,
}

/// One read of the global settings.json serving both consumers (the
/// statusLine config and the `sessionDir` setting used by the transcript
/// latch) so a refresh tick touches the file once.
#[derive(Debug, Clone, Default)]
pub struct SettingsSnapshot {
    pub status_line: Option<StatusLineConfig>,
    pub session_dir: Option<String>,
}

/// Read `<agentDir>/settings.json` (default `~/.rpi/agent/settings.json`)
/// and extract the fields this plugin consumes. Any read/parse failure
/// yields an empty snapshot (plugin no-ops), never an error path.
pub fn load_settings_snapshot() -> SettingsSnapshot {
    read_settings_file(&get_agent_dir().join("settings.json"))
}

/// File-backed variant for tests.
pub(crate) fn read_settings_file(path: &Path) -> SettingsSnapshot {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SettingsSnapshot::default();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return SettingsSnapshot::default();
    };
    SettingsSnapshot {
        status_line: root.get(STATUSLINE_KEY).and_then(parse_status_line),
        session_dir: root
            .get("sessionDir")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// FR-A validation: `type` must be exactly `"command"`, `command` a
/// non-empty string; every other key falls back to its default instead of
/// failing.
fn parse_status_line(value: &Value) -> Option<StatusLineConfig> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("command") {
        return None;
    }
    let command = object.get("command")?.as_str()?.trim();
    if command.is_empty() {
        return None;
    }
    let padding = object
        .get("padding")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_PADDING as u64) as usize;
    let refresh_interval_secs = object
        .get("refreshInterval")
        .and_then(Value::as_u64)
        .map(|secs| secs.max(1));
    let placement = Placement::parse(object.get("placement").and_then(Value::as_str));
    let timeout_ms = object
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    Some(StatusLineConfig {
        command: command.to_owned(),
        padding,
        refresh_interval_secs,
        placement,
        timeout_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Option<StatusLineConfig> {
        parse_status_line(&value)
    }

    #[test]
    fn valid_command_config_parses_with_defaults() {
        let config = parse(json!({
            "type": "command",
            "command": "python3 ~/.claude/statusline.py"
        }))
        .expect("valid config");
        assert_eq!(config.command, "python3 ~/.claude/statusline.py");
        assert_eq!(config.padding, 0);
        assert_eq!(config.refresh_interval_secs, None);
        assert_eq!(config.placement, Placement::Replace);
        assert_eq!(config.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn full_config_parses_with_clamps() {
        let config = parse(json!({
            "type": "command",
            "command": "echo hi",
            "padding": 99,
            "refreshInterval": 0,
            "placement": "status",
            "timeoutMs": 10
        }))
        .expect("valid config");
        assert_eq!(config.padding, MAX_PADDING);
        assert_eq!(config.refresh_interval_secs, Some(1));
        assert_eq!(config.placement, Placement::Status);
        assert_eq!(config.timeout_ms, MIN_TIMEOUT_MS);
        assert_eq!(
            parse(json!({"type": "command", "command": "x", "timeoutMs": 999_999}))
                .unwrap()
                .timeout_ms,
            MAX_TIMEOUT_MS
        );
    }

    #[test]
    fn widget_placement_parses() {
        assert_eq!(
            parse(json!({"type": "command", "command": "x", "placement": "widget"}))
                .unwrap()
                .placement,
            Placement::Widget
        );
    }

    #[test]
    fn invalid_configs_yield_none() {
        // Missing key / wrong type / empty command / non-object.
        assert_eq!(parse(json!({"command": "x"})), None);
        assert_eq!(parse(json!({"type": "progress", "command": "x"})), None);
        assert_eq!(parse(json!({"type": "command"})), None);
        assert_eq!(parse(json!({"type": "command", "command": "  "})), None);
        assert_eq!(parse(json!("nonsense")), None);
        // Unknown placement string falls back to Replace, not an error.
        assert_eq!(
            parse(json!({"type": "command", "command": "x", "placement": "??"}))
                .unwrap()
                .placement,
            Placement::Replace
        );
    }

    #[test]
    fn settings_file_roundtrip_extracts_both_consumers() {
        let dir =
            std::env::temp_dir().join(format!("rpi-statusline-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "theme": "dark",
                "sessionDir": "/data/sessions",
                "statusLine": {"type": "command", "command": "echo ok", "padding": 2}
            }))
            .expect("serialize"),
        )
        .expect("write");
        let snapshot = read_settings_file(&path);
        assert_eq!(
            snapshot.status_line,
            Some(StatusLineConfig {
                command: "echo ok".to_owned(),
                padding: 2,
                refresh_interval_secs: None,
                placement: Placement::Replace,
                timeout_ms: DEFAULT_TIMEOUT_MS,
            })
        );
        assert_eq!(snapshot.session_dir.as_deref(), Some("/data/sessions"));
        // Missing file / broken JSON yield empty snapshots.
        assert_eq!(
            read_settings_file(&dir.join("absent.json")).status_line,
            None
        );
        std::fs::write(&path, b"{ not json").expect("write");
        assert_eq!(read_settings_file(&path).status_line, None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
