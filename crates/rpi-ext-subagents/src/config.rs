//! Extension config + settings keys (P0 subset).
//!
//! Port of pi-subagents `src/extension/config.ts` @ v0.48.0 (56f97234) and the
//! settings keys `subagents.*` the plugin consumes itself (design §4: the ABI
//! has no settings accessor, so the plugin reads the same two settings.json
//! files the host reads, read-only).
//!
//! Upstream semantics preserved:
//! - config.json at `<agentDir>/extensions/subagent/config.json`; unreadable or
//!   invalid → the whole config is dropped (warn + empty), never a hard failure
//!   (config.ts:101-109).
//! - `artifactDir` must be project|session|temp, `maxActiveAsyncRunsPerSession`
//!   and `artifactConfig.cleanupDays` must be non-negative integers — invalid
//!   values invalidate the whole config (config.ts:49-68). P2 keys with object
//!   shapes (missions/authorityPolicy/…) are accepted only as JSON objects.
//! - Settings `subagents.*` are fail-fast: wrong types throw (agents.ts
//!   `readSubagentSettings`). The P0 override-entry subset is
//!   `{description?, model?, disabled?, tools?}`; other upstream override
//!   fields warn and are ignored until TE05 (deviation TE-D15).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::paths;

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionConfig {
    /// `toolDescriptionMode`: full (default) | compact | custom. Invalid values
    /// warn and fall back to `full` at description-build time, like upstream.
    pub tool_description_mode: Option<String>,
    /// `asyncByDefault` (default true). Parsed in P0; effective only from
    /// FR-P1-04 — P0 runs are always foreground (requirements §3.1).
    pub async_by_default: Option<bool>,
    /// Global default run timeout (positive integer ≤ i32::MAX; larger or
    /// invalid values are treated as unset, executor 2267-2270).
    pub timeout_ms: Option<Value>,
    pub default_session_dir: Option<String>,
    pub max_subagent_depth: Option<Value>,
    pub max_subagent_spawns_per_run: Option<Value>,
    pub artifact_dir: Option<String>,
    pub cleanup_days: Option<u64>,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionConfig {
    pub fn new() -> Self {
        Self {
            tool_description_mode: None,
            async_by_default: None,
            timeout_ms: None,
            default_session_dir: None,
            max_subagent_depth: None,
            max_subagent_spawns_per_run: None,
            artifact_dir: None,
            cleanup_days: None,
        }
    }

    /// `artifactDir` normalized with the upstream default `"project"`.
    pub fn artifact_dir_preference(&self) -> &str {
        self.artifact_dir.as_deref().unwrap_or("project")
    }

    /// `artifactConfig.cleanupDays` with the upstream default 7
    /// (DEFAULT_ARTIFACT_CONFIG, types.ts:1958-1967).
    pub fn cleanup_days_or_default(&self) -> u64 {
        self.cleanup_days.unwrap_or(7)
    }

    /// `resolveAsyncByDefault` (config.ts:97-99): anything but `false` is true.
    /// P0 parses the key but always runs foreground; TE05 consumes this.
    #[allow(dead_code)]
    pub fn resolve_async_by_default(&self) -> bool {
        self.async_by_default != Some(false)
    }

    /// `resolveConfigDefaultTimeoutMs` (executor 2267-2270): positive integer
    /// ≤ 2_147_483_647, else unset.
    pub fn resolve_default_timeout_ms(&self) -> Option<u64> {
        match &self.timeout_ms {
            Some(Value::Number(n)) if n.is_u64() || n.is_i64() => {
                let value = n.as_u64().or_else(|| n.as_i64().map(|v| v as u64));
                match value {
                    Some(v) if v > 0 && v <= 2_147_483_647 => Some(v),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// `getConfigPath` (config.ts:70-72) with the ADR-0001 root swap.
pub fn get_config_path() -> PathBuf {
    paths::get_agent_dir()
        .join("extensions")
        .join("subagent")
        .join("config.json")
}

/// `validateConfig` + `readConfigForUpdate` (config.ts:49-82), P0 key subset.
fn parse_config(raw: &Value, path: &str) -> Result<ExtensionConfig, String> {
    let Some(object) = raw.as_object() else {
        return Err(format!("Subagent config at '{path}' must be a JSON object"));
    };
    let mut config = ExtensionConfig::new();
    if let Some(dir) = object.get("artifactDir") {
        let Some(dir) = dir.as_str() else {
            return Err("config.artifactDir must be \"project\", \"session\", or \"temp\"".into());
        };
        if !matches!(dir, "project" | "session" | "temp") {
            return Err("config.artifactDir must be \"project\", \"session\", or \"temp\"".into());
        }
        config.artifact_dir = Some(dir.to_string());
    }
    if let Some(mode) = object.get("toolDescriptionMode") {
        match mode {
            Value::String(s) => config.tool_description_mode = Some(s.clone()),
            _ => return Err("config.toolDescriptionMode must be a string".into()),
        }
    }
    if let Some(flag) = object.get("asyncByDefault") {
        match flag {
            Value::Bool(b) => config.async_by_default = Some(*b),
            _ => return Err("config.asyncByDefault must be a boolean".into()),
        }
    }
    for (key, target) in [
        ("timeoutMs", &mut config.timeout_ms),
        ("maxSubagentDepth", &mut config.max_subagent_depth),
        (
            "maxSubagentSpawnsPerRun",
            &mut config.max_subagent_spawns_per_run,
        ),
    ] {
        if let Some(value) = object.get(key) {
            if !value.is_number() && !value.is_string() {
                return Err(format!("config.{key} must be a number"));
            }
            *target = Some(value.clone());
        }
    }
    if let Some(dir) = object.get("defaultSessionDir") {
        match dir {
            Value::String(s) => config.default_session_dir = Some(s.clone()),
            _ => return Err("config.defaultSessionDir must be a string".into()),
        }
    }
    if let Some(limit) = object.get("maxActiveAsyncRunsPerSession") {
        let valid = limit.as_u64().is_some();
        if !valid {
            return Err(
                "config.maxActiveAsyncRunsPerSession must be a non-negative integer".into(),
            );
        }
    }
    if let Some(artifact) = object.get("artifactConfig") {
        let Some(artifact) = artifact.as_object() else {
            return Err("config.artifactConfig must be a JSON object".into());
        };
        if let Some(days) = artifact.get("cleanupDays") {
            match days.as_u64() {
                Some(days) => config.cleanup_days = Some(days),
                None => {
                    return Err(
                        "config.artifactConfig.cleanupDays must be a non-negative integer".into(),
                    )
                }
            }
        }
    }
    // P2 object-shaped keys only get their JSON-object shape validated, exactly
    // like the upstream validators do before their own field checks.
    for key in [
        "missions",
        "authorityPolicy",
        "permissions",
        "scheduledRuns",
        "fleetKeybindings",
    ] {
        if let Some(value) = object.get(key) {
            if !value.is_object() {
                return Err(format!("config.{key} must be a JSON object"));
            }
        }
    }
    Ok(config)
}

/// `loadConfig` (config.ts:101-109): missing file → empty; unreadable/invalid →
/// warn and return empty (the whole config is discarded).
pub fn load_config() -> ExtensionConfig {
    let path = get_config_path();
    let Some(raw) = read_json_file(&path) else {
        return ExtensionConfig::new();
    };
    match parse_config(&raw, &path.to_string_lossy()) {
        Ok(config) => config,
        Err(message) => {
            tracing::warn!(path = %path.to_string_lossy(), %message, "failed to load subagent config");
            ExtensionConfig::new()
        }
    }
}

fn read_json_file(path: &std::path::Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// P0 subset of `subagents.agentOverrides.<name>` entry fields
/// (BuiltinAgentOverrideConfig subset; TE-D15 narrows to these until TE05).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOverride {
    pub description: Option<String>,
    /// `model: false` clears the agent's model (upstream semantics).
    pub model: Option<Option<String>>,
    pub disabled: Option<bool>,
    pub tools: Option<Option<Vec<String>>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentSettings {
    pub default_model: Option<String>,
    pub disable_builtins: Option<bool>,
    pub overrides: BTreeMap<String, AgentOverride>,
}

/// `readSubagentSettings` (agents.ts:860-928) — fail-fast on invalid values.
/// P0 keys: `defaultModel`, `disableBuiltins`, `agentOverrides.<name>.{
/// description, model, disabled, tools}`. Unknown override fields warn and are
/// ignored (TE-D15); invalid types are errors like upstream.
pub fn read_subagent_settings(path: &std::path::Path) -> Result<SubagentSettings, String> {
    let Some(raw) = read_json_file(path) else {
        return Ok(SubagentSettings::default());
    };
    let Some(object) = raw.as_object() else {
        return Err(format!(
            "Failed to read settings file '{}': must be a JSON object",
            path.to_string_lossy()
        ));
    };
    let Some(subagents) = object.get("subagents") else {
        return Ok(SubagentSettings::default());
    };
    let Some(subagents) = subagents.as_object() else {
        return Ok(SubagentSettings::default());
    };
    let mut parsed = SubagentSettings::default();
    if let Some(value) = subagents.get("disableBuiltins") {
        match value.as_bool() {
            Some(b) => parsed.disable_builtins = Some(b),
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'disableBuiltins'; expected a boolean.",
                    path.to_string_lossy()
                ))
            }
        }
    }
    if let Some(value) = subagents.get("defaultModel") {
        match value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(m) => parsed.default_model = Some(m.to_string()),
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'defaultModel'; expected a non-empty string.",
                    path.to_string_lossy()
                ))
            }
        }
    }
    if let Some(overrides) = subagents.get("agentOverrides") {
        let Some(overrides) = overrides.as_object() else {
            return Ok(parsed);
        };
        for (name, value) in overrides {
            let Some(entry) = value.as_object() else {
                return Err(format!(
                    "Subagent settings in '{}' have invalid agentOverrides entry '{name}'; expected a JSON object.",
                    path.to_string_lossy()
                ));
            };
            let mut parsed_entry = AgentOverride::default();
            let mut ignored = Vec::new();
            for (key, field) in entry {
                match key.as_str() {
                    "description" => {
                        parsed_entry.description = Some(
                            field
                                .as_str()
                                .ok_or_else(|| {
                                    invalid_override_field(path, name, "description", "a string")
                                })?
                                .to_string(),
                        )
                    }
                    "model" => {
                        parsed_entry.model = Some(match field {
                            Value::Bool(false) => None,
                            Value::String(s) => Some(s.clone()),
                            _ => {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "model",
                                    "a string or false",
                                ))
                            }
                        });
                    }
                    "disabled" => {
                        parsed_entry.disabled = Some(field.as_bool().ok_or_else(|| {
                            invalid_override_field(path, name, "disabled", "a boolean")
                        })?);
                    }
                    "tools" => {
                        parsed_entry.tools = Some(match field {
                            Value::Bool(false) => None,
                            Value::Array(items) => {
                                let mut names = Vec::new();
                                for item in items {
                                    names.push(
                                        item.as_str()
                                            .ok_or_else(|| {
                                                invalid_override_field(
                                                    path,
                                                    name,
                                                    "tools",
                                                    "an array of strings or false",
                                                )
                                            })?
                                            .to_string(),
                                    );
                                }
                                Some(names)
                            }
                            _ => {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "tools",
                                    "an array of strings or false",
                                ))
                            }
                        });
                    }
                    _ => ignored.push(key.clone()),
                }
            }
            if !ignored.is_empty() {
                tracing::warn!(
                    agent = %name,
                    fields = ?ignored,
                    "agentOverrides fields beyond the P0 subset are ignored until TE05 (TE-D15)"
                );
            }
            parsed.overrides.insert(name.clone(), parsed_entry);
        }
    }
    Ok(parsed)
}

fn invalid_override_field(
    path: &std::path::Path,
    name: &str,
    field: &str,
    expected: &str,
) -> String {
    format!(
        "Subagent settings in '{}' have invalid agentOverrides.{name}.{field}; expected {expected}.",
        path.to_string_lossy()
    )
}

/// The two settings scopes the plugin reads (`<agentDir>/settings.json` user,
/// `<projectRoot>/.rpi/settings.json` project) plus the merged P0 values.
/// Upstream is fail-fast on invalid values (agents.ts `readSubagentSettings`
/// throws); the plugin downgrades that to warn + empty settings so a bad file
/// cannot brick the whole extension load (deviation TE-D15: same relaxation
/// `loadConfig` applies to its own file).
#[derive(Debug, Clone, Default)]
pub struct SettingsPair {
    pub user: SubagentSettings,
    pub project: SubagentSettings,
    /// Merged `subagents.defaultModel`, project wins
    /// (`resolveSubagentDefaultModel`, agents.ts:944-953).
    pub default_model: Option<String>,
    /// Project `disableBuiltins:true` with a project settings file present
    /// masks the user level (agents.ts:1058-1059).
    pub project_bulk_disabled: bool,
    pub user_bulk_disabled: bool,
    /// Top-level `sessionDir` (project wins) — used to locate the parent
    /// session storage the way the host does (rpi config.rs:195-228).
    pub session_dir: Option<String>,
}

/// Read both settings files. Invalid files warn and contribute nothing.
pub fn read_settings_pair(cwd: &std::path::Path) -> SettingsPair {
    let user_path = paths::get_agent_dir().join("settings.json");
    let user = read_subagent_settings(&user_path).unwrap_or_else(|message| {
        tracing::warn!(path = %user_path.to_string_lossy(), %message, "invalid subagents settings");
        SubagentSettings::default()
    });
    let project_path = crate::agents::project_settings_path(cwd);
    let project = project_path
        .as_ref()
        .and_then(|p| {
            read_subagent_settings(p).map_err(|message| {
                tracing::warn!(path = %p.to_string_lossy(), %message, "invalid subagents settings");
            })
            .ok()
        })
        .unwrap_or_default();
    let project_bulk_disabled = project.disable_builtins == Some(true) && project_path.is_some();
    let user_bulk_disabled =
        project.disable_builtins.is_none() && user.disable_builtins == Some(true);
    let read_session_dir = |path: &std::path::Path| -> Option<String> {
        read_json_file(path)?
            .get("sessionDir")?
            .as_str()
            .map(str::to_string)
    };
    let session_dir = project_path
        .as_ref()
        .and_then(|p| read_session_dir(p))
        .or_else(|| read_session_dir(&user_path));
    SettingsPair {
        default_model: project
            .default_model
            .clone()
            .or_else(|| user.default_model.clone()),
        project_bulk_disabled,
        user_bulk_disabled,
        session_dir,
        user,
        project,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<ExtensionConfig, String> {
        let value: Value = serde_json::from_str(raw).unwrap();
        parse_config(&value, "/cfg/config.json")
    }

    #[test]
    fn empty_config_is_default() {
        let config = parse("{}").unwrap();
        assert_eq!(config.artifact_dir_preference(), "project");
        assert_eq!(config.cleanup_days_or_default(), 7);
        assert!(config.resolve_async_by_default());
        assert_eq!(config.resolve_default_timeout_ms(), None);
    }

    #[test]
    fn invalid_artifact_dir_invalidates_whole_config() {
        assert!(parse(r#"{"artifactDir":"nope"}"#).is_err());
    }

    #[test]
    fn timeout_validation_ignores_out_of_range_at_resolve_time() {
        let config = parse(r#"{"timeoutMs":0}"#).unwrap();
        assert_eq!(config.resolve_default_timeout_ms(), None);
        let config = parse(r#"{"timeoutMs":5000}"#).unwrap();
        assert_eq!(config.resolve_default_timeout_ms(), Some(5000));
        let config = parse(r#"{"timeoutMs":3000000000}"#).unwrap();
        assert_eq!(config.resolve_default_timeout_ms(), None);
    }

    #[test]
    fn cleanup_days_must_be_non_negative_integer() {
        assert!(parse(r#"{"artifactConfig":{"cleanupDays":-1}}"#).is_err());
        assert!(parse(r#"{"artifactConfig":{"cleanupDays":3}}"#).is_ok());
    }

    #[test]
    fn settings_parse_p0_subset_and_fail_fast_on_types() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"subagents":{"disableBuiltins":true,"defaultModel":"m1","agentOverrides":{
                "researcher":{"tools":["read","write"]},
                "scout":{"model":false,"disabled":true},
                "worker":{"systemPromptMode":"append"}
            }}}"#,
        )
        .unwrap();
        let settings = read_subagent_settings(&path).unwrap();
        assert_eq!(settings.disable_builtins, Some(true));
        assert_eq!(settings.default_model.as_deref(), Some("m1"));
        assert_eq!(
            settings.overrides.get("researcher").unwrap().tools,
            Some(Some(vec!["read".to_string(), "write".to_string()]))
        );
        assert_eq!(settings.overrides.get("scout").unwrap().model, Some(None));
        // systemPromptMode outside the P0 subset is ignored (TE-D15), not fatal.
        assert_eq!(settings.overrides.get("worker").unwrap().disabled, None);
        std::fs::write(&path, r#"{"subagents":{"disableBuiltins":"yes"}}"#).unwrap();
        assert!(read_subagent_settings(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
