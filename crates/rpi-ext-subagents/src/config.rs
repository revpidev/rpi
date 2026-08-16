//! Extension config + settings keys (P0 subset + the P1/TE05 additions).
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
//! - Settings `subagents.*` are fail-fast on wrong types (agents.ts
//!   `readSubagentSettings`); the whole file degrades to warn + empty (TE-D15).
//! - TE05 adds the P1 field set: settings keys `defaultThinking`,
//!   `disableThinking`, `defaultExtensions`, `modelScope` and the full
//!   agentOverrides entries; config keys `forceTopLevelAsync`,
//!   `globalConcurrencyLimit`, `maxSubagentSpawnsPerSession`,
//!   `maxActiveAsyncRunsPerSession`, `singleRunOutputBaseDir`, `parallel`,
//!   `waitTool`, `worktreeBaseDir`, `worktreeSetupHook`, `intercomBridge`.
//!   Override fields beyond the documented set (extensions/
//!   subagentOnlyExtensions/completionGuard/toolBudget) warn and are ignored.

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
    /// `artifactConfig.includeJsonl` (execution.ts:1517-1519: on unless
    /// explicitly `false`) — whether child raw event streams land on disk.
    pub artifacts_include_jsonl: Option<bool>,
    /// `forceTopLevelAsync` (default false; FR-P1-04): depth-0 runs forced
    /// into background mode unless `foregroundOnly` (top-level-async.ts:10-12).
    pub force_top_level_async: Option<bool>,
    /// `globalConcurrencyLimit` (default 20; parallel-utils.ts:131) — the
    /// cross-run semaphore cap for parallel children (FR-P1-01/04).
    pub global_concurrency_limit: Option<Value>,
    /// `maxSubagentSpawnsPerSession` (default unlimited; env
    /// `RPI_SUBAGENT_MAX_SPAWNS_PER_SESSION`, FR-P1-04).
    pub max_subagent_spawns_per_session: Option<Value>,
    /// `maxActiveAsyncRunsPerSession` (default unlimited; validated as a
    /// non-negative integer at load, config.ts:56-62).
    pub max_active_async_runs_per_session: Option<u64>,
    /// `singleRunOutputBaseDir` (FR-P1-04): root for relative per-run output
    /// paths when set.
    pub single_run_output_base_dir: Option<String>,
    /// `parallel` (FR-P1-01): `{ maxTasks?, concurrency? }` — raw values kept
    /// because upstream normalizes number-or-numeric-string at resolve time
    /// (`normalizeTopLevelParallelValue`, types.ts:2056-2059).
    pub parallel_max_tasks: Option<Value>,
    pub parallel_concurrency: Option<Value>,
    /// `waitTool` (FR-P1-04): boolean or `{ enabled?: boolean }`; env
    /// `RPI_SUBAGENT_WAIT_TOOL_ENABLED` overrides (wait-config.ts:30-34).
    pub wait_tool: Option<Value>,
    /// `fleet` (TE11 FR-C): `{ enabled?, expanded? }` — the persistent fleet
    /// status widget below the editor. `expanded` shows the per-run tree
    /// instead of the one-line summary (no keyboard interaction: the widget
    /// is read-only; TE11 Out — inspector/navigation).
    pub fleet: Option<Value>,
    /// `worktreeBaseDir` (FR-P1-06): base directory for managed worktrees;
    /// env `RPI_SUBAGENTS_WORKTREE_DIR` overrides; default system temp.
    pub worktree_base_dir: Option<String>,
    /// `worktreeSetupHook` (FR-P1-06): command string or
    /// `{ command, timeoutMs? }` (default 30s, worktree.ts:115).
    pub worktree_setup_hook: Option<Value>,
    /// `intercomBridge` (FR-P1-10): `{ mode?: "always"|"fork-only"|"off",
    /// instructionFile?, resultDelivery? }` (intercom-bridge.ts:82-89).
    pub intercom_bridge: Option<Value>,
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
            artifacts_include_jsonl: None,
            force_top_level_async: None,
            global_concurrency_limit: None,
            max_subagent_spawns_per_session: None,
            max_active_async_runs_per_session: None,
            single_run_output_base_dir: None,
            parallel_max_tasks: None,
            parallel_concurrency: None,
            wait_tool: None,
            fleet: None,
            worktree_base_dir: None,
            worktree_setup_hook: None,
            intercom_bridge: None,
        }
    }

    /// `artifactDir` normalized with the upstream default `"project"`.
    pub fn artifact_dir_preference(&self) -> &str {
        self.artifact_dir.as_deref().unwrap_or("project")
    }

    /// `artifactConfig.includeJsonl` with the upstream default `true`
    /// (execution.ts:1517-1519 — the raw child event stream is written
    /// unless the config says `false`).
    pub fn include_jsonl(&self) -> bool {
        self.artifacts_include_jsonl.unwrap_or(true)
    }

    /// `artifactConfig.cleanupDays` with the upstream default 7    /// (DEFAULT_ARTIFACT_CONFIG, types.ts:1958-1967).
    pub fn cleanup_days_or_default(&self) -> u64 {
        self.cleanup_days.unwrap_or(7)
    }

    /// `resolveAsyncByDefault` (config.ts:97-99): anything but `false` is true.
    // Consumed by the FR-P1-04 background wave of this same task (TE05).
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

    /// `normalizeTopLevelParallelValue` (types.ts:2056-2059): integer ≥ 1 as
    /// number or numeric string, else `None`.
    fn normalize_parallel_value(value: Option<&Value>) -> Option<u64> {
        let value = value?;
        let parsed = match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }?;
        if parsed.fract() != 0.0 || parsed < 1.0 {
            return None;
        }
        Some(parsed as u64)
    }

    /// `resolveTopLevelParallelMaxTasks` (types.ts:2061-2063): default 8.
    pub fn parallel_max_tasks(&self) -> u64 {
        Self::normalize_parallel_value(self.parallel_max_tasks.as_ref()).unwrap_or(8)
    }

    /// `resolveTopLevelParallelConcurrency` (types.ts:2065+): call override >
    /// config value > default 4.
    pub fn parallel_concurrency(&self, call_override: Option<&Value>) -> u64 {
        Self::normalize_parallel_value(call_override)
            .or_else(|| Self::normalize_parallel_value(self.parallel_concurrency.as_ref()))
            .unwrap_or(4)
    }

    /// `resolveWaitToolConfig` (wait-config.ts:30-34): env value (validated
    /// truthy/falsy word set) > configured boolean/object > true. Invalid env
    /// values upstream throw; here they warn and fall through so a bad env
    /// cannot brick tool registration (load-time relaxation, TE-D15 style).
    #[allow(dead_code)]
    pub fn wait_tool_enabled(&self) -> bool {
        const ENV: &str = "RPI_SUBAGENT_WAIT_TOOL_ENABLED";
        if let Ok(raw) = std::env::var(ENV) {
            let normalized = raw.trim().to_lowercase();
            match normalized.as_str() {
                "1" | "true" | "yes" | "on" | "enabled" => return true,
                "0" | "false" | "no" | "off" | "disabled" => return false,
                _ => {
                    tracing::warn!(env = %ENV, value = %raw, "invalid waitTool env value; ignoring")
                }
            }
        }
        match &self.wait_tool {
            Some(Value::Bool(b)) => *b,
            Some(Value::Object(object)) => object
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            _ => true,
        }
    }

    /// `fleet.enabled` (TE11 FR-C): env
    /// `RPI_SUBAGENT_FLEET_ENABLED` > config > true.
    pub fn fleet_enabled(&self) -> bool {
        const ENV: &str = "RPI_SUBAGENT_FLEET_ENABLED";
        if let Ok(raw) = std::env::var(ENV) {
            let normalized = raw.trim().to_lowercase();
            match normalized.as_str() {
                "1" | "true" | "yes" | "on" | "enabled" => return true,
                "0" | "false" | "no" | "off" | "disabled" => return false,
                _ => {
                    tracing::warn!(env = %ENV, value = %raw, "invalid fleet env value; ignoring")
                }
            }
        }
        match &self.fleet {
            Some(Value::Bool(b)) => *b,
            Some(Value::Object(object)) => object
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            _ => true,
        }
    }

    /// `fleet.expanded` (TE11 FR-C): config > false. The widget is
    /// read-only, so the tree form is config-driven (no keyboard toggle).
    pub fn fleet_expanded(&self) -> bool {
        match &self.fleet {
            Some(Value::Object(object)) => object
                .get("expanded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            _ => false,
        }
    }

    /// `globalConcurrencyLimit` with the upstream default 20
    /// (DEFAULT_GLOBAL_CONCURRENCY_LIMIT, parallel-utils.ts:131).
    #[allow(dead_code)]
    pub fn global_concurrency_limit(&self) -> u64 {
        match &self.global_concurrency_limit {
            Some(Value::Number(n)) => n.as_u64().filter(|v| *v >= 1).unwrap_or(20),
            Some(Value::String(s)) => s
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|v| *v >= 1)
                .unwrap_or(20),
            _ => 20,
        }
    }

    /// `maxSubagentSpawnsPerSession` (FR-P1-04): env
    /// `RPI_SUBAGENT_MAX_SPAWNS_PER_SESSION` > config value; `None` =
    /// unlimited.
    #[allow(dead_code)]
    pub fn max_subagent_spawns_per_session(&self) -> Option<u64> {
        let parse = |value: Option<&Value>| -> Option<u64> {
            match value {
                Some(Value::Number(n)) => n.as_u64(),
                Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
                _ => None,
            }
        };
        if let Ok(raw) = std::env::var("RPI_SUBAGENT_MAX_SPAWNS_PER_SESSION") {
            if let Some(parsed) = parse(Some(&Value::String(raw.clone()))) {
                return Some(parsed);
            }
            tracing::warn!(value = %raw, "invalid RPI_SUBAGENT_MAX_SPAWNS_PER_SESSION; ignoring");
        }
        parse(self.max_subagent_spawns_per_session.as_ref()).filter(|v| *v > 0)
    }

    /// `maxActiveAsyncRunsPerSession`; `None` = unlimited (types.ts:1838).
    #[allow(dead_code)]
    pub fn max_active_async_runs_per_session(&self) -> Option<u64> {
        self.max_active_async_runs_per_session.filter(|v| *v > 0)
    }

    /// `worktreeSetupHook` resolved to `(command, timeout_ms)` (worktree.ts
    /// `runWorktreeSetupHook` L336-379 + default 30000ms at L115). Accepts the
    /// string form documented in extension/index.ts:12 and the object form.
    #[allow(dead_code)]
    pub fn worktree_setup_hook(&self) -> Option<(String, u64)> {
        let value = self.worktree_setup_hook.as_ref()?;
        let (command, timeout) = match value {
            Value::String(command) => (Some(command.clone()), None),
            Value::Object(object) => (
                object
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                object.get("timeoutMs").and_then(Value::as_u64),
            ),
            _ => (None, None),
        };
        let command = command.filter(|c| !c.trim().is_empty())?;
        Some((command, timeout.unwrap_or(30_000)))
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
        config.max_active_async_runs_per_session = limit.as_u64();
    }
    // P1 keys are consumed at their use sites with upstream defaults, so they
    // only get light shape checks here (upstream validateConfig does not
    // check them either).
    if let Some(flag) = object.get("forceTopLevelAsync") {
        match flag.as_bool() {
            Some(b) => config.force_top_level_async = Some(b),
            None => return Err("config.forceTopLevelAsync must be a boolean".into()),
        }
    }
    if let Some(value) = object.get("globalConcurrencyLimit") {
        if !value.is_number() && !value.is_string() {
            return Err("config.globalConcurrencyLimit must be a number".into());
        }
        config.global_concurrency_limit = Some(value.clone());
    }
    if let Some(value) = object.get("maxSubagentSpawnsPerSession") {
        if !value.is_number() && !value.is_string() {
            return Err("config.maxSubagentSpawnsPerSession must be a number".into());
        }
        config.max_subagent_spawns_per_session = Some(value.clone());
    }
    if let Some(dir) = object.get("singleRunOutputBaseDir") {
        match dir.as_str().filter(|s| !s.trim().is_empty()) {
            Some(dir) => config.single_run_output_base_dir = Some(dir.to_string()),
            None => return Err("config.singleRunOutputBaseDir must be a non-empty string".into()),
        }
    }
    if let Some(parallel) = object.get("parallel") {
        let Some(parallel) = parallel.as_object() else {
            return Err("config.parallel must be a JSON object".into());
        };
        for (key, target) in [
            ("maxTasks", &mut config.parallel_max_tasks),
            ("concurrency", &mut config.parallel_concurrency),
        ] {
            if let Some(value) = parallel.get(key) {
                if !value.is_number() && !value.is_string() {
                    return Err(format!("config.parallel.{key} must be a number"));
                }
                *target = Some(value.clone());
            }
        }
    }
    if let Some(wait_tool) = object.get("waitTool") {
        // wait-config.ts:22-29: boolean, or object whose `enabled` (when
        // present) is a boolean.
        let valid = wait_tool.is_boolean()
            || wait_tool
                .as_object()
                .is_some_and(|o| match o.get("enabled") {
                    None => true,
                    Some(enabled) => enabled.is_boolean(),
                });
        if !valid {
            return Err(
                "config.waitTool must be a boolean or an object with optional enabled boolean"
                    .into(),
            );
        }
        config.wait_tool = Some(wait_tool.clone());
    }
    if let Some(fleet) = object.get("fleet") {
        // TE11 FR-C: boolean, or `{enabled?, expanded?}` with boolean values.
        let valid = fleet.is_boolean()
            || fleet.as_object().is_some_and(|o| {
                o.len() <= 2
                    && o.get("enabled").is_none_or(Value::is_boolean)
                    && o.get("expanded").is_none_or(Value::is_boolean)
            });
        if !valid {
            return Err(
                "config.fleet must be a boolean or an object with optional enabled/expanded booleans"
                    .into(),
            );
        }
        config.fleet = Some(fleet.clone());
    }
    if let Some(dir) = object.get("worktreeBaseDir") {
        match dir.as_str().filter(|s| !s.trim().is_empty()) {
            Some(dir) => config.worktree_base_dir = Some(dir.to_string()),
            None => return Err("config.worktreeBaseDir must be a non-empty string".into()),
        }
    }
    if let Some(hook) = object.get("worktreeSetupHook") {
        let valid = hook.as_str().is_some_and(|s| !s.trim().is_empty())
            || hook
                .as_object()
                .is_some_and(|o| o.get("command").and_then(Value::as_str).is_some());
        if !valid {
            return Err(
                "config.worktreeSetupHook must be a command string or an object with a command"
                    .into(),
            );
        }
        config.worktree_setup_hook = Some(hook.clone());
    }
    if let Some(bridge) = object.get("intercomBridge") {
        let Some(bridge_object) = bridge.as_object() else {
            return Err("config.intercomBridge must be a JSON object".into());
        };
        if let Some(mode) = bridge_object.get("mode") {
            if !mode
                .as_str()
                .is_some_and(|m| matches!(m, "off" | "always" | "fork-only"))
            {
                return Err(
                    "config.intercomBridge.mode must be \"always\", \"fork-only\", or \"off\""
                        .into(),
                );
            }
        }
        config.intercom_bridge = Some(bridge.clone());
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
        if let Some(include_jsonl) = artifact.get("includeJsonl") {
            match include_jsonl.as_bool() {
                Some(value) => config.artifacts_include_jsonl = Some(value),
                None => return Err("config.artifactConfig.includeJsonl must be a boolean".into()),
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

/// `subagents.agentOverrides.<name>` entry — the P1 field set of
/// `BuiltinAgentOverrideConfig` (agents.ts `parseBuiltinOverrideEntry`
/// L756-898; requirements §3.2 P1 全集). `false` variants clear the agent's
/// own value; `None` leaves it untouched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOverride {
    pub description: Option<String>,
    /// `model: false` clears the agent's model (upstream semantics).
    pub model: Option<Option<String>>,
    pub disabled: Option<bool>,
    pub tools: Option<Option<Vec<String>>>,
    // —— P1 fields (TE05; requirements §3.2 全集) ——
    pub fallback_models: Option<Option<Vec<String>>>,
    /// `thinking: false` clears the agent's thinking level.
    pub thinking: Option<Option<String>>,
    /// `append` | `replace`.
    pub system_prompt_mode: Option<String>,
    pub inherit_project_context: Option<bool>,
    pub inherit_skills: Option<bool>,
    /// `fresh` | `fork` | false (clear).
    pub default_context: Option<Option<String>>,
    /// `read-only` | `writer` | false (clear) — FR-P1-09.
    pub acceptance_role: Option<Option<String>>,
    pub system_prompt: Option<String>,
    pub skills: Option<Option<Vec<String>>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentSettings {
    pub default_model: Option<String>,
    pub disable_builtins: Option<bool>,
    pub overrides: BTreeMap<String, AgentOverride>,
    // —— P1 keys (TE05) ——
    pub default_thinking: Option<String>,
    pub disable_thinking: Option<bool>,
    pub default_extensions: Option<Vec<String>>,
    pub model_scope: Option<crate::launch::model::ModelScopeConfig>,
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
    // —— P1 keys (agents.ts:877-916) ——
    if let Some(value) = subagents.get("disableThinking") {
        match value.as_bool() {
            Some(b) => parsed.disable_thinking = Some(b),
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'disableThinking'; expected a boolean.",
                    path.to_string_lossy()
                ))
            }
        }
    }
    if let Some(value) = subagents.get("defaultThinking") {
        match value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(t) => parsed.default_thinking = Some(t.to_string()),
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'defaultThinking'; expected a non-empty string.",
                    path.to_string_lossy()
                ))
            }
        }
    }
    if let Some(value) = subagents.get("defaultExtensions") {
        match value.as_array() {
            Some(items) => {
                let mut names = Vec::new();
                let mut valid = true;
                for item in items {
                    match item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(name) => names.push(name.to_string()),
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid {
                    return Err(format!(
                        "Subagent settings in '{}' have invalid 'defaultExtensions'; expected an array of non-empty strings.",
                        path.to_string_lossy()
                    ));
                }
                parsed.default_extensions = Some(names);
            }
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'defaultExtensions'; expected an array of non-empty strings.",
                    path.to_string_lossy()
                ))
            }
        }
    }
    match crate::launch::model::parse_model_scope_config(subagents.get("modelScope")) {
        Ok(scope) => parsed.model_scope = scope,
        Err(message) => {
            return Err(format!(
                "Subagent settings in '{}' {}",
                path.to_string_lossy(),
                message
            ))
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
                    "fallbackModels" => {
                        parsed_entry.fallback_models = Some(parse_override_string_array_or_false(
                            field,
                            path,
                            name,
                            "fallbackModels",
                        )?);
                    }
                    "thinking" => {
                        parsed_entry.thinking = Some(match field {
                            Value::Bool(false) => None,
                            Value::String(s) => Some(s.clone()),
                            _ => {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "thinking",
                                    "a string or false",
                                ))
                            }
                        });
                    }
                    "systemPromptMode" => {
                        parsed_entry.system_prompt_mode = Some(match field.as_str() {
                            Some("append") | Some("replace") => field.as_str().unwrap().to_string(),
                            _ => {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "systemPromptMode",
                                    "'append' or 'replace'",
                                ))
                            }
                        });
                    }
                    "inheritProjectContext" => {
                        parsed_entry.inherit_project_context =
                            Some(field.as_bool().ok_or_else(|| {
                                invalid_override_field(
                                    path,
                                    name,
                                    "inheritProjectContext",
                                    "a boolean",
                                )
                            })?);
                    }
                    "inheritSkills" => {
                        parsed_entry.inherit_skills = Some(field.as_bool().ok_or_else(|| {
                            invalid_override_field(path, name, "inheritSkills", "a boolean")
                        })?);
                    }
                    "defaultContext" => {
                        parsed_entry.default_context = Some(match field {
                            Value::Bool(false) => None,
                            Value::String(s) if s == "fresh" || s == "fork" => Some(s.clone()),
                            _ => {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "defaultContext",
                                    "'fresh', 'fork', or false",
                                ))
                            }
                        });
                    }
                    "acceptanceRole" => {
                        parsed_entry.acceptance_role = Some(match field {
                            Value::Bool(false) => None,
                            Value::String(s) if s == "read-only" || s == "writer" => {
                                Some(s.clone())
                            }
                            _ => {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "acceptanceRole",
                                    "'read-only', 'writer', or false",
                                ))
                            }
                        });
                    }
                    "systemPrompt" => {
                        parsed_entry.system_prompt = Some(
                            field
                                .as_str()
                                .ok_or_else(|| {
                                    invalid_override_field(path, name, "systemPrompt", "a string")
                                })?
                                .to_string(),
                        );
                    }
                    "skills" => {
                        parsed_entry.skills = Some(parse_override_string_array_or_false(
                            field, path, name, "skills",
                        )?);
                    }
                    _ => ignored.push(key.clone()),
                }
            }
            if !ignored.is_empty() {
                tracing::warn!(
                    agent = %name,
                    fields = ?ignored,
                    "agentOverrides fields beyond the documented set are ignored (extensions/subagentOnlyExtensions/completionGuard/toolBudget stay unsupported)"
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

/// `parseOverrideStringArrayOrFalse` (agents.ts:741-757): array of non-empty
/// strings or `false` (clear); anything else is a settings error.
fn parse_override_string_array_or_false(
    field: &Value,
    path: &std::path::Path,
    name: &str,
    field_name: &str,
) -> Result<Option<Vec<String>>, String> {
    match field {
        Value::Bool(false) => Ok(None),
        Value::Array(items) => {
            let mut names = Vec::new();
            for item in items {
                let Some(item) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                    return Err(invalid_override_field(
                        path,
                        name,
                        field_name,
                        "an array of non-empty strings or false",
                    ));
                };
                names.push(item.to_string());
            }
            Ok(Some(names))
        }
        _ => Err(invalid_override_field(
            path,
            name,
            field_name,
            "an array of non-empty strings or false",
        )),
    }
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
    // —— P1 merges (TE05) ——
    /// `resolveSubagentDefaultThinking` (agents.ts:986-992): project wins.
    pub default_thinking: Option<String>,
    /// `disableThinking` with the project-configured mask
    /// (`projectThinkingConfigured`, agents.ts:1060-1064).
    pub disable_thinking: bool,
    /// `projectThinkingConfigured`: a project settings file exists and sets
    /// the `disableThinking` key — this also cancels user-override thinking
    /// protection (agents.ts:1085).
    pub project_thinking_configured: bool,
    /// `resolveSubagentDefaultExtensions` (agents.ts:1013-1019): project wins.
    pub default_extensions: Option<Vec<String>>,
    /// `modelScope` — project wins when a project settings file exists
    /// (same mask discipline as the other defaults).
    pub model_scope: Option<crate::launch::model::ModelScopeConfig>,
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
    // `resolveSubagentDefaultThinking` / `resolveSubagentDefaultExtensions`
    // (agents.ts:986-1019): a project settings file that sets the key masks
    // the user level. `disableThinking` follows the projectThinkingConfigured
    // mask (agents.ts:1060-1064).
    let project_settings_present = project_path.is_some();
    let default_thinking = if project_settings_present && project.default_thinking.is_some() {
        project.default_thinking.clone()
    } else {
        user.default_thinking.clone()
    };
    let default_extensions = if project_settings_present && project.default_extensions.is_some() {
        project.default_extensions.clone()
    } else {
        user.default_extensions.clone()
    };
    let disable_thinking = if project_settings_present && project.disable_thinking.is_some() {
        project.disable_thinking == Some(true)
    } else {
        user.disable_thinking == Some(true)
    };
    let project_thinking_configured =
        project_settings_present && project.disable_thinking.is_some();
    let model_scope = if project_settings_present && project.model_scope.is_some() {
        project.model_scope.clone()
    } else {
        user.model_scope.clone()
    };
    SettingsPair {
        default_model: project
            .default_model
            .clone()
            .or_else(|| user.default_model.clone()),
        project_bulk_disabled,
        user_bulk_disabled,
        session_dir,
        default_thinking,
        disable_thinking,
        project_thinking_configured,
        default_extensions,
        model_scope,
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
    fn settings_parse_p1_fields_and_fail_fast_on_types() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-p1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"subagents":{
                "disableBuiltins":true,"defaultModel":"m1",
                "defaultThinking":"high","disableThinking":false,
                "defaultExtensions":["a.wasm","b.wasm"],
                "modelScope":{"enforce":true,"allow":["anthropic/*","openai/*"]},
                "agentOverrides":{
                    "researcher":{"tools":["read","write"]},
                    "scout":{"model":false,"disabled":true},
                    "worker":{"systemPromptMode":"append","fallbackModels":["m2","m3"],
                              "thinking":false,"defaultContext":"fork","acceptanceRole":"writer",
                              "skills":["s1"],"systemPrompt":"custom","inheritSkills":false},
                    "reviewer":{"completionGuard":true}
                }
            }}"#,
        )
        .unwrap();
        let settings = read_subagent_settings(&path).unwrap();
        assert_eq!(settings.disable_builtins, Some(true));
        assert_eq!(settings.default_model.as_deref(), Some("m1"));
        assert_eq!(settings.default_thinking.as_deref(), Some("high"));
        assert_eq!(settings.disable_thinking, Some(false));
        assert_eq!(
            settings.default_extensions,
            Some(vec!["a.wasm".to_string(), "b.wasm".to_string()])
        );
        let scope = settings.model_scope.unwrap();
        assert!(scope.enforced());
        assert_eq!(
            scope.allow,
            Some(vec!["anthropic/*".to_string(), "openai/*".to_string()])
        );
        assert_eq!(
            settings.overrides.get("researcher").unwrap().tools,
            Some(Some(vec!["read".to_string(), "write".to_string()]))
        );
        assert_eq!(settings.overrides.get("scout").unwrap().model, Some(None));
        let worker = settings.overrides.get("worker").unwrap();
        assert_eq!(worker.system_prompt_mode.as_deref(), Some("append"));
        assert_eq!(
            worker.fallback_models,
            Some(Some(vec!["m2".to_string(), "m3".to_string()]))
        );
        assert_eq!(worker.thinking, Some(None));
        assert_eq!(worker.default_context, Some(Some("fork".to_string())));
        assert_eq!(worker.acceptance_role, Some(Some("writer".to_string())));
        assert_eq!(worker.skills, Some(Some(vec!["s1".to_string()])));
        assert_eq!(worker.system_prompt.as_deref(), Some("custom"));
        assert_eq!(worker.inherit_skills, Some(false));
        // completionGuard is beyond the documented set → ignored, not fatal.
        assert!(settings
            .overrides
            .get("reviewer")
            .unwrap()
            .disabled
            .is_none());
        std::fs::write(&path, r#"{"subagents":{"disableBuiltins":"yes"}}"#).unwrap();
        assert!(read_subagent_settings(&path).is_err());
        std::fs::write(&path, r#"{"subagents":{"defaultThinking":""}}"#).unwrap();
        assert!(read_subagent_settings(&path).is_err());
        std::fs::write(&path, r#"{"subagents":{"modelScope":{"enforce":"yes"}}}"#).unwrap();
        assert!(read_subagent_settings(&path).is_err());
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverrides":{"x":{"thinking":5}}}}"#,
        )
        .unwrap();
        assert!(read_subagent_settings(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p1_config_keys_parse_and_resolve() {
        let config = parse(
            r#"{"parallel":{"maxTasks":"6","concurrency":2},"waitTool":{"enabled":false},
                "forceTopLevelAsync":true,"globalConcurrencyLimit":"12",
                "maxSubagentSpawnsPerSession":100,"maxActiveAsyncRunsPerSession":3,
                "singleRunOutputBaseDir":"out","worktreeBaseDir":"/tmp/wt",
                "worktreeSetupHook":"./setup.sh",
                "intercomBridge":{"mode":"fork-only"}}"#,
        )
        .unwrap();
        assert_eq!(config.parallel_max_tasks(), 6);
        assert_eq!(config.parallel_concurrency(None), 2);
        assert_eq!(
            config.parallel_concurrency(Some(&serde_json::json!("3"))),
            3
        );
        // defaults
        let empty = parse("{}").unwrap();
        assert_eq!(empty.parallel_max_tasks(), 8);
        assert_eq!(empty.parallel_concurrency(None), 4);
        assert_eq!(empty.global_concurrency_limit(), 20);
        assert_eq!(config.global_concurrency_limit(), 12);
        assert_eq!(config.max_active_async_runs_per_session(), Some(3));
        assert_eq!(empty.max_active_async_runs_per_session(), None);
        assert_eq!(config.max_subagent_spawns_per_session(), Some(100));
        assert_eq!(empty.max_subagent_spawns_per_session(), None);
        assert_eq!(config.single_run_output_base_dir.as_deref(), Some("out"));
        assert_eq!(config.worktree_base_dir.as_deref(), Some("/tmp/wt"));
        assert_eq!(
            config.worktree_setup_hook(),
            Some(("./setup.sh".to_string(), 30_000))
        );
        assert!(config.force_top_level_async == Some(true));
        // invalid values invalidate the whole config (upstream style).
        assert!(parse(r#"{"waitTool":"yes"}"#).is_err());
        assert!(parse(r#"{"intercomBridge":{"mode":"weird"}}"#).is_err());
        assert!(parse(r#"{"parallel":{"maxTasks":0}}"#).is_ok()); // resolved at use site
        assert_eq!(
            parse(r#"{"parallel":{"maxTasks":0}}"#)
                .unwrap()
                .parallel_max_tasks(),
            8
        );
    }

    #[test]
    fn wait_tool_env_resolution() {
        let config = parse(r#"{"waitTool":{"enabled":false}}"#).unwrap();
        // config disabled → false; env true overrides.
        assert!(!config.wait_tool_enabled());
        std::env::set_var("RPI_SUBAGENT_WAIT_TOOL_ENABLED", "1");
        assert!(config.wait_tool_enabled());
        std::env::set_var("RPI_SUBAGENT_WAIT_TOOL_ENABLED", "bogus");
        // invalid env values warn and fall through to the config value.
        assert!(!config.wait_tool_enabled());
        std::env::remove_var("RPI_SUBAGENT_WAIT_TOOL_ENABLED");
    }

    #[test]
    fn settings_pair_p1_merges_project_wins() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-pair-{}", std::process::id()));
        let user_dir = dir.join("user");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            user_dir.join("settings.json"),
            r#"{"subagents":{"defaultThinking":"low","disableThinking":true,
                "modelScope":{"enforce":true,"allow":["user/*"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            project_dir.join("settings.json"),
            r#"{"subagents":{"defaultThinking":"high","disableThinking":false}}"#,
        )
        .unwrap();
        let user_settings = read_subagent_settings(&user_dir.join("settings.json")).unwrap();
        let project_settings = read_subagent_settings(&project_dir.join("settings.json")).unwrap();
        // Reproduce the pair merge with explicit paths (read_settings_pair
        // resolves paths from cwd; here we test the merge arithmetic only).
        let project_present = true;
        let default_thinking = if project_present && project_settings.default_thinking.is_some() {
            project_settings.default_thinking.clone()
        } else {
            user_settings.default_thinking.clone()
        };
        assert_eq!(default_thinking.as_deref(), Some("high"));
        let disable_thinking = if project_present && project_settings.disable_thinking.is_some() {
            project_settings.disable_thinking == Some(true)
        } else {
            user_settings.disable_thinking == Some(true)
        };
        assert!(!disable_thinking);
        let model_scope = if project_present && project_settings.model_scope.is_some() {
            project_settings.model_scope.clone()
        } else {
            user_settings.model_scope.clone()
        };
        // Project file present but without modelScope → the user-level scope
        // still applies (only an explicit project value masks it).
        assert_eq!(model_scope.and_then(|s| s.allow).map(|a| a.len()), Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
