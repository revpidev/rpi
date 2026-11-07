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

use crate::items::{ItemsConfig, parse_items};
use crate::paths::get_agent_dir;

/// Top-level settings key (CC-compatible spelling).
pub const STATUSLINE_KEY: &str = "statusLine";

/// `timeoutMs` bounds (rpi extension; CC documents no timeout).
pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
pub const MIN_TIMEOUT_MS: u64 = 500;
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// `statusLine.liveTokens` bounds (rpi extension,
/// 03-realtime-token-count §1.5): streaming re-run throttle.
pub const DEFAULT_LIVE_REFRESH_MS: u64 = 1000;
pub const MIN_LIVE_REFRESH_MS: u64 = 300;
pub const MAX_LIVE_REFRESH_MS: u64 = 5000;

/// Parsed `statusLine.liveTokens` (bool | object). Both `enabled` and
/// `refresh_ms` are re-read on every poll/tick: enabling or disabling the
/// switch reconciles the streaming subscriptions dynamically (no restart),
/// and `refresh_ms` applies immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveTokens {
    pub enabled: bool,
    pub refresh_ms: u64,
}

impl Default for LiveTokens {
    fn default() -> Self {
        LiveTokens {
            enabled: false,
            refresh_ms: DEFAULT_LIVE_REFRESH_MS,
        }
    }
}

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
    /// host joins extension statuses onto one row with ` · ` and folds
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

    /// Items-mode placement parser: `status` is the default (the token rows
    /// sit on the shared bottom row with rpi-usage), an unknown value falls
    /// back to it.
    pub(crate) fn parse_items(value: &str) -> Self {
        match value {
            "replace" => Placement::Replace,
            "widget" => Placement::Widget,
            _ => Placement::Status,
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
    /// `statusLine.liveTokens` (rpi extension; 03-realtime-token-count
    /// §1.5). `enabled` here reflects the CURRENT settings read; the refresh
    /// loop reconciles the subscription set from it on every poll.
    pub live_tokens: LiveTokens,
}

/// One parsed `statusLine` mode (TE52: the two modes are mutually
/// exclusive; only the selected `type` runs).
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLine {
    /// TE12 script mode — unchanged stdin/timeout/failure semantics.
    Command(StatusLineConfig),
    /// Declarative 30-field mode (TE52 / ADR-0040).
    Items(ItemsConfig),
}

impl StatusLine {
    pub fn placement(&self) -> Placement {
        match self {
            Self::Command(config) => config.placement,
            Self::Items(config) => config.placement,
        }
    }

    pub fn padding(&self) -> usize {
        match self {
            Self::Command(config) => config.padding,
            Self::Items(config) => config.padding,
        }
    }

    /// Command mode has no default timer (`None` = event-driven only); items
    /// mode always carries a local refresh interval.
    pub fn refresh_interval_secs(&self) -> Option<u64> {
        match self {
            Self::Command(config) => config.refresh_interval_secs,
            Self::Items(config) => Some(config.refresh_interval_secs),
        }
    }

    pub fn live_tokens(&self) -> LiveTokens {
        match self {
            Self::Command(config) => config.live_tokens,
            Self::Items(config) => config.live_tokens,
        }
    }
}

/// One read of the global settings.json serving both consumers (the
/// statusLine config and the `sessionDir` setting used by the transcript
/// latch) so a refresh tick touches the file once.
#[derive(Debug, Clone, Default)]
pub struct SettingsSnapshot {
    pub status_line: Option<StatusLine>,
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

/// FR-A validation: the parsed mode is selected exclusively by `type`;
/// `command` requires a non-empty command, `items` accepts the declarative
/// object with per-key fallbacks. Any other type disables the plugin
/// (missing/invalid config never touches the footer).
fn parse_status_line(value: &Value) -> Option<StatusLine> {
    let object = value.as_object()?;
    match object.get("type").and_then(Value::as_str) {
        Some("command") => parse_command(object).map(StatusLine::Command),
        Some("items") => Some(StatusLine::Items(parse_items(object))),
        _ => None,
    }
}

fn parse_command(object: &serde_json::Map<String, Value>) -> Option<StatusLineConfig> {
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
    let live_tokens = parse_live_tokens(object.get("liveTokens"));
    Some(StatusLineConfig {
        command: command.to_owned(),
        padding,
        refresh_interval_secs,
        placement,
        timeout_ms,
        live_tokens,
    })
}

/// §1.5: `true` = on with defaults; an object may override `refreshMs`
/// (clamped 300..5000, out-of-range/invalid falls back to the default);
/// `false` = off. The missing/wrong-type default is caller-specific: command
/// mode defaults to off, while the items default install enables the session
/// token total plus the live rate row (see [`parse_live_tokens_with_default`]).
/// Both modes reconcile the subscription set dynamically.
pub(crate) fn parse_live_tokens(value: Option<&Value>) -> LiveTokens {
    parse_live_tokens_with_default(value, false)
}

/// Parse `liveTokens` with an explicit missing/malformed default. Command
/// mode defaults to off; items mode defaults to on (the default install is
/// the session token total plus the live rate row).
pub(crate) fn parse_live_tokens_with_default(
    value: Option<&Value>,
    default_enabled: bool,
) -> LiveTokens {
    let mut live = LiveTokens {
        enabled: true,
        ..LiveTokens::default()
    };
    match value {
        Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => live.enabled = false,
        Some(Value::Object(object)) => {
            if let Some(refresh) = object.get("refreshMs").and_then(Value::as_u64) {
                live.refresh_ms = refresh.clamp(MIN_LIVE_REFRESH_MS, MAX_LIVE_REFRESH_MS);
            }
        }
        _ => {
            live.enabled = default_enabled;
        }
    }
    live
}

/// Load-time subscription evaluation for COMMAND mode (§1.5 启用时机注记):
/// whether `liveTokens` is enabled in the current command configuration.
/// Items mode reconciles live subscriptions dynamically instead.
pub fn command_live_tokens_configured_in_global_settings() -> bool {
    matches!(
        load_settings_snapshot().status_line,
        Some(StatusLine::Command(config)) if config.live_tokens.enabled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::FieldId;
    use serde_json::json;

    fn parse(value: Value) -> Option<StatusLine> {
        parse_status_line(&value)
    }

    fn command(value: Value) -> StatusLineConfig {
        match parse(value).expect("valid config") {
            StatusLine::Command(config) => config,
            other => panic!("expected command mode, got {other:?}"),
        }
    }

    #[test]
    fn valid_command_config_parses_with_defaults() {
        let config = command(json!({
            "type": "command",
            "command": "python3 ~/.claude/statusline.py"
        }));
        assert_eq!(config.command, "python3 ~/.claude/statusline.py");
        assert_eq!(config.padding, 0);
        assert_eq!(config.refresh_interval_secs, None);
        assert_eq!(config.placement, Placement::Replace);
        assert_eq!(config.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(config.live_tokens, LiveTokens::default());
    }

    #[test]
    fn full_config_parses_with_clamps() {
        let config = command(json!({
            "type": "command",
            "command": "echo hi",
            "padding": 99,
            "refreshInterval": 0,
            "placement": "status",
            "timeoutMs": 10
        }));
        assert_eq!(config.padding, MAX_PADDING);
        assert_eq!(config.refresh_interval_secs, Some(1));
        assert_eq!(config.placement, Placement::Status);
        assert_eq!(config.timeout_ms, MIN_TIMEOUT_MS);
        assert_eq!(
            command(json!({"type": "command", "command": "x", "timeoutMs": 999_999})).timeout_ms,
            MAX_TIMEOUT_MS
        );
    }

    #[test]
    fn widget_placement_parses() {
        assert_eq!(
            command(json!({"type": "command", "command": "x", "placement": "widget"})).placement,
            Placement::Widget
        );
    }

    #[test]
    fn items_mode_parses_exclusively_and_keeps_command_ignored() {
        // `type` selects the mode; a retained command string does not run.
        let mode = parse(json!({
            "type": "items",
            "command": "echo old",
            "items": ["model", "project"]
        }))
        .expect("items config");
        let StatusLine::Items(config) = mode else {
            panic!("expected items mode");
        };
        let fields: Vec<FieldId> = config.fields().collect();
        assert_eq!(fields, vec![FieldId::Model, FieldId::ProjectName]);
        // The default items placement is status (command keeps replace).
        assert_eq!(config.placement, Placement::Status);
    }

    #[test]
    fn unknown_type_disables_the_plugin() {
        assert_eq!(parse(json!({"type": "items-x", "items": []})), None);
    }

    #[test]
    fn live_tokens_parses_bool_object_and_clamps() {
        for off in [None, Some(json!(false)), Some(json!("yes")), Some(json!(1))] {
            let value = off.clone();
            let config = command(json!({"type": "command", "command": "x"}));
            assert!(!config.live_tokens.enabled);
            assert_eq!(parse_live_tokens(value.as_ref()), LiveTokens::default());
        }
        assert_eq!(
            parse_live_tokens(Some(&json!(true))),
            LiveTokens {
                enabled: true,
                refresh_ms: DEFAULT_LIVE_REFRESH_MS,
            }
        );
        assert_eq!(
            parse_live_tokens(Some(&json!({"refreshMs": 500}))),
            LiveTokens {
                enabled: true,
                refresh_ms: 500,
            }
        );
        assert_eq!(
            parse_live_tokens(Some(&json!({"refreshMs": 1}))).refresh_ms,
            MIN_LIVE_REFRESH_MS
        );
        assert_eq!(
            parse_live_tokens(Some(&json!({"refreshMs": 999_999}))).refresh_ms,
            MAX_LIVE_REFRESH_MS
        );
        assert_eq!(
            parse_live_tokens(Some(&json!({"refreshMs": "soon"}))).refresh_ms,
            DEFAULT_LIVE_REFRESH_MS
        );
        assert_eq!(
            parse_live_tokens(Some(&json!({}))),
            LiveTokens {
                enabled: true,
                refresh_ms: DEFAULT_LIVE_REFRESH_MS,
            }
        );
    }

    #[test]
    fn invalid_configs_yield_none() {
        assert_eq!(parse(json!({"command": "x"})), None);
        assert_eq!(parse(json!({"type": "progress", "command": "x"})), None);
        assert_eq!(parse(json!({"type": "command"})), None);
        assert_eq!(parse(json!({"type": "command", "command": "  "})), None);
        assert_eq!(parse(json!("nonsense")), None);
        assert_eq!(
            command(json!({"type": "command", "command": "x", "placement": "??"})).placement,
            Placement::Replace
        );
    }

    #[test]
    fn mode_helpers_project_the_frozen_defaults() {
        let items = parse(json!({"type": "items"})).expect("items");
        assert_eq!(items.placement(), Placement::Status);
        assert_eq!(items.padding(), 0);
        assert_eq!(items.refresh_interval_secs(), Some(5));
        assert!(items.live_tokens().enabled);
        let command = parse(json!({"type": "command", "command": "echo x"})).expect("command");
        assert_eq!(command.placement(), Placement::Replace);
        assert_eq!(command.refresh_interval_secs(), None);
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
            Some(StatusLine::Command(StatusLineConfig {
                command: "echo ok".to_owned(),
                padding: 2,
                refresh_interval_secs: None,
                placement: Placement::Replace,
                timeout_ms: DEFAULT_TIMEOUT_MS,
                live_tokens: LiveTokens::default(),
            }))
        );
        assert_eq!(snapshot.session_dir.as_deref(), Some("/data/sessions"));
        // Items mode round-trips through the same snapshot.
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "statusLine": {"type": "items", "items": ["model"], "liveTokens": true}
            }))
            .expect("serialize"),
        )
        .expect("write");
        let snapshot = read_settings_file(&path);
        let Some(StatusLine::Items(config)) = snapshot.status_line else {
            panic!("expected items mode");
        };
        assert_eq!(config.fields().collect::<Vec<_>>(), vec![FieldId::Model]);
        assert!(config.live_tokens.enabled);
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
