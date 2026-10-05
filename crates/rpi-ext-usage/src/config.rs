//! The plugin-owned `usage` settings (TE44 FR-E; plugin 01 §7).
//!
//! The host's usage-provider framework types only the two keys it consumes
//! itself (`usage.providers`, `usage.timeoutMs`; V16-05 §7.3 item 8). The
//! plugin-owned keys — `enabled`, `footer`, `refreshMs` — are read directly
//! from the same `<agentDir>/settings.json` the host reads (statusline
//! precedent, avoiding a second source of truth).
//!
//! Fail-soft contract: a missing file, broken JSON, or a wrong-typed key
//! falls back to the default for that key only; unknown keys are ignored. The
//! file is re-read on every trigger, so edits apply immediately (hot reload).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Default footer refresh throttle (plugin 01 §7 `refreshMs`).
pub const DEFAULT_REFRESH_MS: u64 = 300_000;
/// Lower bound for `usage.refreshMs` (avoid hammering the framework; the
/// framework cache TTL is 60s).
pub const MIN_REFRESH_MS: u64 = 1_000;
/// Upper bound for `usage.refreshMs`.
pub const MAX_REFRESH_MS: u64 = 3_600_000;

/// Resolved plugin configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageConfig {
    /// Master switch (`usage.enabled`, default `true`).
    pub enabled: bool,
    /// Footer status line switch (`usage.footer`, default `true`).
    pub footer: bool,
    /// `message_end` throttle window in milliseconds (`usage.refreshMs`,
    /// clamped 1s..1h; default 5min).
    pub refresh_ms: u64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            footer: true,
            refresh_ms: DEFAULT_REFRESH_MS,
        }
    }
}

/// Home directory (`$HOME` on Unix, `%USERPROFILE%` on Windows — the
/// rpi-todo / rpi-plan-mode convention).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// `RPI_CODING_AGENT_DIR` else `~/.rpi/agent` (ADR-0001; the statusline /
/// mcp-adapter derivation). Unit tests may pin an isolated directory.
pub fn agent_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(slot) = TEST_AGENT_DIR.get()
            && let Some(pinned) = slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        {
            return pinned;
        }
    }
    match std::env::var_os("RPI_CODING_AGENT_DIR") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => match home_dir() {
            Some(home) => home.join(".rpi").join("agent"),
            None => PathBuf::from(".rpi").join("agent"),
        },
    }
}

#[cfg(test)]
static TEST_AGENT_DIR: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

/// Test seam: pin the agent dir (`None` restores the environment rule).
#[cfg(test)]
pub fn set_test_agent_dir(dir: Option<PathBuf>) {
    let slot = TEST_AGENT_DIR.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = dir;
}

/// The plugin section of the settings file (`usage`).
pub fn load() -> UsageConfig {
    load_from_path(&agent_dir().join("settings.json"))
}

/// File-backed variant (tests and explicit paths).
pub fn load_from_path(path: &Path) -> UsageConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return UsageConfig::default();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return UsageConfig::default();
    };
    parse_settings(&root)
}

/// Extract the `usage` section (fail-soft per key).
pub fn parse_settings(root: &Value) -> UsageConfig {
    let defaults = UsageConfig::default();
    let Some(section) = root.get("usage").and_then(Value::as_object) else {
        return defaults;
    };
    let mut config = defaults;
    if let Some(value) = section.get("enabled") {
        match value.as_bool() {
            Some(enabled) => config.enabled = enabled,
            None => tracing::warn!("rpi-usage: usage.enabled must be a boolean"),
        }
    }
    if let Some(value) = section.get("footer") {
        match value.as_bool() {
            Some(footer) => config.footer = footer,
            None => tracing::warn!("rpi-usage: usage.footer must be a boolean"),
        }
    }
    if let Some(value) = section.get("refreshMs") {
        match value.as_u64() {
            Some(ms) => config.refresh_ms = ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS),
            None => tracing::warn!("rpi-usage: usage.refreshMs must be a number"),
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_the_requirement_table() {
        let config = UsageConfig::default();
        assert!(config.enabled);
        assert!(config.footer);
        assert_eq!(config.refresh_ms, DEFAULT_REFRESH_MS);
    }

    #[test]
    fn parses_overrides_and_ignores_unknown_keys() {
        let config = parse_settings(&json!({
            "theme": "dark",
            "usage": {
                "enabled": false,
                "footer": false,
                "refreshMs": 120000,
                "providers": { "x": "y" },
                "timeoutMs": 3000,
                "futureKey": true
            }
        }));
        assert_eq!(
            config,
            UsageConfig {
                enabled: false,
                footer: false,
                refresh_ms: 120_000,
            }
        );
    }

    #[test]
    fn refresh_ms_clamps_and_type_errors_fall_back_per_key() {
        assert_eq!(
            parse_settings(&json!({"usage": {"refreshMs": 5}})).refresh_ms,
            MIN_REFRESH_MS
        );
        assert_eq!(
            parse_settings(&json!({"usage": {"refreshMs": 999_999_999}})).refresh_ms,
            MAX_REFRESH_MS
        );
        let config = parse_settings(&json!({
            "usage": {"enabled": "yes", "footer": false, "refreshMs": "soon"}
        }));
        assert!(config.enabled, "bad enabled falls back to true");
        assert!(!config.footer, "valid key survives");
        assert_eq!(config.refresh_ms, DEFAULT_REFRESH_MS);
    }

    #[test]
    fn missing_or_broken_settings_answer_defaults() {
        let dir = std::env::temp_dir().join(format!("rpi-usage-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("settings.json");
        assert_eq!(load_from_path(&path), UsageConfig::default(), "missing");
        std::fs::write(&path, b"{ not json").expect("write");
        assert_eq!(load_from_path(&path), UsageConfig::default(), "broken");
        std::fs::write(&path, serde_json::to_string(&json!({"usage": {}})).unwrap())
            .expect("write");
        assert_eq!(
            load_from_path(&path),
            UsageConfig::default(),
            "empty section"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
