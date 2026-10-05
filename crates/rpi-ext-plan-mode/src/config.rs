//! XDG configuration for `rpi-plan-mode` (TE43 FR-A; TE-D45).
//!
//! Path: `$XDG_CONFIG_HOME/rpi-plan-mode/config.toml`, falling back to
//! `~/.config/rpi-plan-mode/config.toml` when `XDG_CONFIG_HOME` is unset,
//! empty, whitespace-only, or relative (the rpi-todo `resolveConfigDir`
//! precedent). The path is intentionally brand-independent (TE-D45, the
//! documented [VARIANT] from the ADR-0001 `~/.rpi` default).
//!
//! Fail-soft contract (plugin 01 §7): a missing file or directory yields
//! the defaults; a TOML parse error yields the defaults plus a warning;
//! unknown keys are ignored; a type error on one key falls back for that
//! key only and does not discard the rest of the file.
//!
//! The file is re-read on every mode reconcile (TE43 §8-6), so an edit
//! takes effect at the next trigger (mode change / session event /
//! `before_agent_start`).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Default Plan-mode read-only whitelist (plugin 01 §7 `allowTools`):
/// built-in read tools plus the first-party web retrieval tools. `bash` is
/// deliberately absent (01 §3 R-PM-2.3: no command classifier yet).
pub const DEFAULT_ALLOW_TOOLS: &[&str] =
    &["read", "grep", "find", "ls", "web_fetch", "batch_web_fetch"];

/// The plugin-owned plan tool name; always present in the Plan-mode active
/// set regardless of the configured allow/block lists (01 §3 R-PM-2.5).
pub const WRITE_PLAN_TOOL: &str = "write_plan";

/// Default plan directory, relative to the session cwd (01 §7 `planDir`).
pub const DEFAULT_PLAN_DIR: &str = ".rpi/plans";

/// Resolved plugin configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanModeConfig {
    /// Plan-mode allow list (`allowTools`).
    pub allow_tools: Vec<String>,
    /// Explicit removals, applied after the allow list (`blockTools`).
    pub block_tools: Vec<String>,
    /// Plan directory override (`planDir`): relative paths resolve against
    /// the session cwd; absolute paths are used verbatim (01 §7:
    /// documented risk, no sandboxing in the first release).
    pub plan_dir: Option<String>,
    /// Whether the plan-oriented prompt injection is enabled
    /// (`promptInjection`); the read-only boundary applies either way.
    pub prompt_injection: bool,
}

impl Default for PlanModeConfig {
    fn default() -> Self {
        Self {
            allow_tools: DEFAULT_ALLOW_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            block_tools: Vec::new(),
            plan_dir: None,
            prompt_injection: true,
        }
    }
}

/// Home directory (`$HOME` on Unix, `%USERPROFILE%` on Windows — the
/// rpi-todo / mcp-adapter convention).
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

/// Expand a leading `~/` (and a bare `~`); `~user` is left untouched.
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

/// `$XDG_CONFIG_HOME/rpi-plan-mode/config.toml`, else
/// `~/.config/rpi-plan-mode/config.toml`; `None` when no home is known.
pub fn config_path() -> Option<PathBuf> {
    let home = home_dir()?;
    let dir = match std::env::var("XDG_CONFIG_HOME") {
        Ok(raw) if !raw.trim().is_empty() => {
            let expanded = expand_tilde(raw.trim(), &home);
            if expanded.is_absolute() {
                expanded
            } else {
                home.join(".config")
            }
        }
        _ => home.join(".config"),
    };
    Some(dir.join("rpi-plan-mode").join("config.toml"))
}

#[cfg(test)]
static TEST_CONFIG: std::sync::OnceLock<std::sync::Mutex<Option<Option<String>>>> =
    std::sync::OnceLock::new();

/// Test seam: pin the on-disk config text (`None` disables the override).
/// `Some(None)` models a missing file.
#[cfg(test)]
pub fn set_test_config(text: Option<Option<String>>) {
    let slot = TEST_CONFIG.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = text;
}

/// Load the config from the resolved XDG path (fail-soft). Test builds use
/// the pinned override when installed.
pub fn load_config() -> PlanModeConfig {
    #[cfg(test)]
    {
        if let Some(slot) = TEST_CONFIG.get() {
            let pinned = slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(text) = pinned {
                return match text {
                    Some(text) => parse_config(&text),
                    None => PlanModeConfig::default(),
                };
            }
        }
    }
    let Some(path) = config_path() else {
        return PlanModeConfig::default();
    };
    load_config_from_path(&path)
}

/// Load from an explicit path (missing file = defaults).
pub fn load_config_from_path(path: &Path) -> PlanModeConfig {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PlanModeConfig::default(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "rpi-plan-mode: config read failed; using defaults");
            PlanModeConfig::default()
        }
    }
}

/// Parse the config text (fail-soft per key; unknown keys ignored).
pub fn parse_config(text: &str) -> PlanModeConfig {
    let value: toml::Value = match toml::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "rpi-plan-mode: config.toml parse failed; using defaults");
            return PlanModeConfig::default();
        }
    };
    let Some(table) = value.as_table() else {
        tracing::warn!("rpi-plan-mode: config.toml root is not a table; using defaults");
        return PlanModeConfig::default();
    };
    let defaults = PlanModeConfig::default();
    let mut config = defaults.clone();

    if let Some(value) = table.get("allowTools") {
        match string_array(value) {
            Some(names) => config.allow_tools = names,
            None => tracing::warn!("rpi-plan-mode: allowTools must be an array of strings"),
        }
    }
    if let Some(value) = table.get("blockTools") {
        match string_array(value) {
            Some(names) => config.block_tools = names,
            None => tracing::warn!("rpi-plan-mode: blockTools must be an array of strings"),
        }
    }
    if let Some(value) = table.get("planDir") {
        match value.as_str() {
            Some(dir) if !dir.trim().is_empty() => config.plan_dir = Some(dir.trim().to_owned()),
            Some(_) => {}
            None => tracing::warn!("rpi-plan-mode: planDir must be a string"),
        }
    }
    if let Some(value) = table.get("promptInjection") {
        match value.as_bool() {
            Some(enabled) => config.prompt_injection = enabled,
            None => tracing::warn!("rpi-plan-mode: promptInjection must be a boolean"),
        }
    }
    config
}

fn string_array(value: &toml::Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|entry| entry.as_str().map(str::to_owned))
        .collect()
}

/// Serialize the effective config (used by `/plan status` and tests).
pub fn config_json(config: &PlanModeConfig) -> Value {
    serde_json::json!({
        "allowTools": config.allow_tools,
        "blockTools": config.block_tools,
        "planDir": config.plan_dir,
        "promptInjection": config.prompt_injection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_requirement_table() {
        let config = PlanModeConfig::default();
        assert_eq!(config.allow_tools, DEFAULT_ALLOW_TOOLS);
        assert!(config.block_tools.is_empty());
        assert_eq!(config.plan_dir, None);
        assert!(config.prompt_injection);
    }

    #[test]
    fn parses_overrides_and_ignores_unknown_keys() {
        let text = r#"
            allowTools = ["read", "bash"]
            blockTools = ["web_fetch"]
            planDir = "/tmp/plans"
            promptInjection = false
            futureKey = "ignored"
        "#;
        let config = parse_config(text);
        assert_eq!(config.allow_tools, vec!["read", "bash"]);
        assert_eq!(config.block_tools, vec!["web_fetch"]);
        assert_eq!(config.plan_dir.as_deref(), Some("/tmp/plans"));
        assert!(!config.prompt_injection);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        let config = parse_config("allowTools = [\"read\"");
        assert_eq!(config, PlanModeConfig::default());
    }

    #[test]
    fn per_key_type_errors_keep_the_other_keys() {
        let text = r#"
            allowTools = "read"
            blockTools = ["edit"]
            planDir = 7
            promptInjection = "yes"
        "#;
        let config = parse_config(text);
        assert_eq!(
            config.allow_tools, DEFAULT_ALLOW_TOOLS,
            "bad key falls back"
        );
        assert_eq!(config.block_tools, vec!["edit"], "valid key survives");
        assert_eq!(config.plan_dir, None);
        assert!(config.prompt_injection);
    }

    #[test]
    fn empty_plan_dir_and_blank_entries_are_normalized() {
        let config = parse_config("planDir = \"  \"");
        assert_eq!(config.plan_dir, None);
    }

    #[test]
    fn resolves_xdg_with_fallback_rules() {
        let home = Path::new("/home/tester");
        // Unset / blank / relative are all the `~/.config` fallback; the
        // absolute form is honored. `resolve` is inlined in `config_path`,
        // so this test mirrors its documented cases through expansion.
        assert_eq!(expand_tilde("~/x", home), home.join("x"));
        assert_eq!(expand_tilde("/abs", home), PathBuf::from("/abs"));
        assert_eq!(expand_tilde("~user/x", home), PathBuf::from("~user/x"));
    }

    #[test]
    fn missing_file_answers_defaults() {
        let config = load_config_from_path(Path::new("/nonexistent/rpi-plan-mode/config.toml"));
        assert_eq!(config, PlanModeConfig::default());
    }

    #[test]
    fn config_json_round_trips_the_four_keys() {
        let config = parse_config(
            "allowTools = [\"read\"]\nblockTools = [\"edit\"]\nplanDir = \"plans\"\npromptInjection = false",
        );
        assert_eq!(
            config_json(&config),
            serde_json::json!({
                "allowTools": ["read"],
                "blockTools": ["edit"],
                "planDir": "plans",
                "promptInjection": false,
            })
        );
    }
}
