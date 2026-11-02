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
//! - Settings `subagents.*` are fail-fast on parse/validation errors
//!   (agents.ts `readSubagentSettings`; R18.9.4 BREAKING). Both user and
//!   trusted-project files abort the extension load; an untrusted project is
//!   never read (`read_settings_pair_trusted`).
//! - TE05 adds the P1 field set: settings keys `defaultThinking`,
//!   `disableThinking`, `defaultExtensions`, `modelScope` and the full
//!   agentOverrides entries; config keys `forceTopLevelAsync`,
//!   `globalConcurrencyLimit`, `maxSubagentSpawnsPerSession`,
//!   `maxActiveAsyncRunsPerSession`, `singleRunOutputBaseDir`, `parallel`,
//!   `waitTool`, `worktreeBaseDir`, `worktreeSetupHook`, `intercomBridge`.
//!   `mutationTools` and `agentOverridesByProvider` are fully parsed; fields
//!   beyond the documented set (extensions/subagentOnlyExtensions/…) warn
//!   and are ignored.

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
    /// `waitTool.defaultTimeoutMs` (#1591): default `subagent_wait` window
    /// when the call omits `timeoutMs` (positive integer; env
    /// `RPI_SUBAGENT_WAIT_TOOL_DEFAULT_TIMEOUT_MS` overrides). None = the
    /// built-in 30-minute default.
    pub wait_tool_default_timeout_ms: Option<u64>,
    /// `control` (subagent-control.ts): global attention/active-long-running
    /// thresholds. Raw JSON because `resolveControlConfig` is lenient and
    /// ignores invalid positive integers/lists.
    pub control: Option<Value>,
    /// `enableSlashCommands` (#2977/#2989): register the plugin's slash
    /// commands. Defaults to true; an explicit false disables registration
    /// and direct command dispatch.
    pub enable_slash_commands: Option<bool>,
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
            wait_tool_default_timeout_ms: None,
            control: None,
            enable_slash_commands: None,
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

    /// `enableSlashCommands` default true (#2977/#2989).
    pub fn slash_commands_enabled(&self) -> bool {
        self.enable_slash_commands.unwrap_or(true)
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

    /// `waitTool.defaultTimeoutMs` (#1591, wait-config.ts
    /// `resolveWaitToolConfig`): env `RPI_SUBAGENT_WAIT_TOOL_DEFAULT_TIMEOUT_MS`
    /// (positive integer) > config > None (the caller's built-in default).
    pub fn wait_tool_default_timeout_ms(&self) -> Option<u64> {
        const ENV: &str = "RPI_SUBAGENT_WAIT_TOOL_DEFAULT_TIMEOUT_MS";
        if let Ok(raw) = std::env::var(ENV) {
            match raw.trim().parse::<u64>() {
                Ok(v) if v >= 1 => return Some(v),
                _ => {
                    tracing::warn!(env = %ENV, value = %raw, "invalid defaultTimeoutMs env value; ignoring")
                }
            }
        }
        self.wait_tool_default_timeout_ms
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
    /// (DEFAULT_GLOBAL_CONCURRENCY_LIMIT, parallel-utils.ts:131). Enforced as
    /// the run-wide child semaphore in `run_parallel_async` (rpi#30).
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
    if let Some(flag) = object.get("enableSlashCommands") {
        match flag {
            Value::Bool(b) => config.enable_slash_commands = Some(*b),
            _ => return Err("config.enableSlashCommands must be a boolean".into()),
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
        // wait-config.ts:22-29 + #1591: boolean, or object whose `enabled`
        // (when present) is a boolean and whose `defaultTimeoutMs` (when
        // present) is a positive integer.
        let valid = wait_tool.is_boolean()
            || wait_tool.as_object().is_some_and(|o| {
                let enabled_ok = match o.get("enabled") {
                    None => true,
                    Some(enabled) => enabled.is_boolean(),
                };
                let timeout_ok = match o.get("defaultTimeoutMs") {
                    None => true,
                    Some(timeout) => timeout.as_u64().is_some_and(|v| v >= 1),
                };
                enabled_ok && timeout_ok
            });
        if !valid {
            return Err(
                "config.waitTool must be a boolean or an object with optional enabled boolean and defaultTimeoutMs positive integer"
                    .into(),
            );
        }
        if let Some(timeout) = wait_tool
            .as_object()
            .and_then(|o| o.get("defaultTimeoutMs"))
            .and_then(Value::as_u64)
            .filter(|v| *v >= 1)
        {
            config.wait_tool_default_timeout_ms = Some(timeout);
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
        if let Some(mode) = bridge_object.get("mode")
            && !mode
                .as_str()
                .is_some_and(|m| matches!(m, "off" | "always" | "fork-only"))
        {
            return Err(
                "config.intercomBridge.mode must be \"always\", \"fork-only\", or \"off\"".into(),
            );
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
                    );
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
        if let Some(value) = object.get(key)
            && !value.is_object()
        {
            return Err(format!("config.{key} must be a JSON object"));
        }
    }
    if let Some(control) = object.get("control") {
        if !control.is_object() && !control.is_null() {
            return Err("config.control must be a JSON object".into());
        }
        config.control = control.as_object().map(|_| control.clone());
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

/// `subagents.agentOverrides.<name>.tools` value (upstream
/// `BuiltinAgentOverrideConfig["tools"]`, agents.ts:103 #1047/#1049):
/// `"inherit"` clears the allowlist back to undeclared, `false` is an
/// explicit empty allowlist, an array is the allowlist.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolsOverride {
    /// `tools: "inherit"` — host defaults (no tool flags).
    Inherit,
    /// `tools: false` — explicit empty allowlist (`--no-tools`).
    Disable,
    /// `tools: ["a", …]` — explicit allowlist (`mcp:` entries split out).
    List(Vec<String>),
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
    /// #1047/#1049 (R7.1.4.1): `"inherit"` / `false` / list tri-state.
    pub tools: Option<ToolsOverride>,
    /// #1776 (R7.1.4.2): deny-list (`false` clears back to undeclared).
    pub exclude_tools: Option<Option<Vec<String>>>,
    // —— P1 fields (TE05; requirements §3.2 全集) ——
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
    /// `systemPrompt` override (agents.ts:1104-1106).
    pub system_prompt: Option<String>,
    /// `advertise` override (#2534, agents.ts:1014-1017 @ b6bda32f): list a
    /// builtin agent in the parent's advertised catalog from settings.
    pub advertise: Option<bool>,
    /// `defaultProvider` builtin override (#1393, agents.ts:1087-1090):
    /// non-empty string sets the agent's preferred provider; `false`
    /// (`Some(None)`) clears it so the settings default does not apply.
    pub default_provider: Option<Option<String>>,
    /// `allowNestedSubagents` override (#1587, agents.ts:1100-1102).
    pub allow_nested_subagents: Option<bool>,
    /// `allowedAgents` override (#2338): descendant agent allowlist
    /// (`Some(None)` clears back to unrestricted).
    pub allowed_agents: Option<Option<Vec<String>>>,
    /// `outputMode` override (#1305, agents.ts:979-983): inline | file-only.
    pub output_mode: Option<String>,
    pub skills: Option<Option<Vec<String>>>,
    /// `mutationTools` (agents.ts:1232): string[] or false (clear).
    pub mutation_tools: Option<Option<Vec<String>>>,
}

impl AgentOverride {
    /// Shallow field merge used by `selectProviderOverrides` (agents.ts:1370-1378):
    /// every field present in `other` wins over `self`.
    pub fn merge_from(&mut self, other: &AgentOverride) {
        if other.description.is_some() {
            self.description = other.description.clone();
        }
        if other.model.is_some() {
            self.model = other.model.clone();
        }
        if other.disabled.is_some() {
            self.disabled = other.disabled;
        }
        if other.tools.is_some() {
            self.tools = other.tools.clone();
        }
        if other.exclude_tools.is_some() {
            self.exclude_tools = other.exclude_tools.clone();
        }
        if other.thinking.is_some() {
            self.thinking = other.thinking.clone();
        }
        if other.system_prompt_mode.is_some() {
            self.system_prompt_mode = other.system_prompt_mode.clone();
        }
        if other.inherit_project_context.is_some() {
            self.inherit_project_context = other.inherit_project_context;
        }
        if other.inherit_skills.is_some() {
            self.inherit_skills = other.inherit_skills;
        }
        if other.default_context.is_some() {
            self.default_context = other.default_context.clone();
        }
        if other.acceptance_role.is_some() {
            self.acceptance_role = other.acceptance_role.clone();
        }
        if other.system_prompt.is_some() {
            self.system_prompt = other.system_prompt.clone();
        }
        if other.advertise.is_some() {
            self.advertise = other.advertise;
        }
        if other.default_provider.is_some() {
            self.default_provider = other.default_provider.clone();
        }
        if other.allow_nested_subagents.is_some() {
            self.allow_nested_subagents = other.allow_nested_subagents;
        }
        if other.allowed_agents.is_some() {
            self.allowed_agents = other.allowed_agents.clone();
        }
        if other.output_mode.is_some() {
            self.output_mode = other.output_mode.clone();
        }
        if other.skills.is_some() {
            self.skills = other.skills.clone();
        }
        if other.mutation_tools.is_some() {
            self.mutation_tools = other.mutation_tools.clone();
        }
    }
}

impl SubagentSettings {
    /// Upstream `selectProviderOverrides` (agents.ts:1370-1378): a provider's
    /// per-agent entries merge over the base `agentOverrides` for that provider.
    pub fn with_provider_overrides(&self, provider: Option<&str>) -> SubagentSettings {
        let Some(provider) = provider else {
            return self.clone();
        };
        let Some(selected) = self.provider_overrides.get(provider) else {
            return self.clone();
        };
        let mut out = self.clone();
        for (name, entry) in selected {
            out.overrides
                .entry(name.clone())
                .or_default()
                .merge_from(entry);
        }
        out
    }
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
    /// `agentOverridesByProvider.<provider>.<agent>` entries (agents.ts:1326-1365).
    pub provider_overrides: BTreeMap<String, BTreeMap<String, AgentOverride>>,
    /// `defaultSubagentOnlyExtensions` (#2322): child-only extension paths
    /// for agents without their own `subagentOnlyExtensions` field; unlike
    /// `defaultExtensions: []` this never disables ambient discovery. An
    /// explicitly present project value wins over the user value; the two
    /// defaults resolve independently.
    pub default_subagent_only_extensions: Option<Vec<String>>,
    pub model_scope: Option<crate::launch::model::ModelScopeConfig>,
    // —— v0.66 keys (TE19 / R7.1.10.x, R7.1.11.x) ——
    /// `maxThinking` (#1397): thinking ceiling across subagent launches
    /// (off|minimal|low|medium|high|xhigh|max); project settings win.
    pub max_thinking: Option<String>,
    /// `defaultProvider` (#1393): preferred provider for bare model ids;
    /// project settings win.
    pub default_provider: Option<String>,
    /// `agentScanDirs` (#1801): extra agent discovery directories
    /// (one-segment `*` wildcard allowed); user-scope and project-scope
    /// settings stay scoped to their own discovery level.
    pub agent_scan_dirs: Option<Vec<String>>,
    /// `agentExcludeDirs` (#2146): directory trees excluded from agent
    /// discovery (resolved against this settings file's directory).
    pub agent_exclude_dirs: Option<Vec<String>>,
}

/// `readSubagentSettings` (agents.ts:860-928) — fail-fast on invalid values.
/// P0 keys: `defaultModel`, `disableBuiltins`, `agentOverrides.<name>.{
/// description, model, disabled, tools}`. Unknown override fields warn and are
/// ignored (TE-D15); invalid types are errors like upstream.
pub fn read_subagent_settings(path: &std::path::Path) -> Result<SubagentSettings, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SubagentSettings::default());
        }
        Err(error) => {
            return Err(format!(
                "Failed to read settings file '{}': {error}",
                path.to_string_lossy()
            ));
        }
    };
    let raw: Value = serde_json::from_str(&content).map_err(|error| {
        format!(
            "Failed to parse settings file '{}': {error}",
            path.to_string_lossy()
        )
    })?;
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
        return Err(format!(
            "Subagent settings in '{}' must be a JSON object.",
            path.to_string_lossy()
        ));
    };
    let mut parsed = SubagentSettings::default();
    if let Some(value) = subagents.get("disableBuiltins") {
        match value.as_bool() {
            Some(b) => parsed.disable_builtins = Some(b),
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'disableBuiltins'; expected a boolean.",
                    path.to_string_lossy()
                ));
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
                ));
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
                ));
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
                ));
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
                ));
            }
        }
    }
    if let Some(value) = subagents.get("defaultSubagentOnlyExtensions") {
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
                        "Subagent settings in '{}' have invalid 'defaultSubagentOnlyExtensions'; expected an array of non-empty strings.",
                        path.to_string_lossy()
                    ));
                }
                parsed.default_subagent_only_extensions = Some(names);
            }
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'defaultSubagentOnlyExtensions'; expected an array of non-empty strings.",
                    path.to_string_lossy()
                ));
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
            ));
        }
    }
    // —— v0.66 keys (TE19) ——
    if let Some(value) = subagents.get("maxThinking") {
        // agents.ts:1164-1170: ThinkingLevel vocabulary, fail-fast.
        match value.as_str().map(str::trim) {
            Some(level)
                if matches!(
                    level,
                    "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                ) =>
            {
                parsed.max_thinking = Some(level.to_string());
            }
            _ => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'maxThinking'; expected one of off, minimal, low, medium, high, xhigh, or max.",
                    path.to_string_lossy()
                ));
            }
        }
    }
    if let Some(value) = subagents.get("defaultProvider") {
        // agents.ts:1148-1154: non-empty string.
        match value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(provider) => parsed.default_provider = Some(provider.to_string()),
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'defaultProvider'; expected a non-empty string.",
                    path.to_string_lossy()
                ));
            }
        }
    }
    if let Some(value) = subagents.get("agentScanDirs") {
        // agents.ts settings parse + agent-scan-dirs.test.ts: array of
        // strings (non-string entries fail the whole settings file like
        // upstream `invalid 'agentScanDirs'`).
        match value.as_array() {
            Some(items) => {
                let mut dirs = Vec::new();
                let mut valid = true;
                for item in items {
                    match item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(dir) => dirs.push(dir.to_string()),
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid {
                    return Err(format!(
                        "Subagent settings in '{}' have invalid 'agentScanDirs'; expected an array of strings.",
                        path.to_string_lossy()
                    ));
                }
                parsed.agent_scan_dirs = Some(dirs);
            }
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'agentScanDirs'; expected an array of strings.",
                    path.to_string_lossy()
                ));
            }
        }
    }

    if let Some(value) = subagents.get("agentExcludeDirs") {
        match value.as_array() {
            Some(items) => {
                let mut dirs = Vec::new();
                let mut valid = true;
                for item in items {
                    match item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(dir) => dirs.push(dir.to_string()),
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid {
                    return Err(format!(
                        "Subagent settings in '{}' have invalid 'agentExcludeDirs'; expected an array of non-empty strings.",
                        path.to_string_lossy()
                    ));
                }
                parsed.agent_exclude_dirs = Some(dirs);
            }
            None => {
                return Err(format!(
                    "Subagent settings in '{}' have invalid 'agentExcludeDirs'; expected an array of non-empty strings.",
                    path.to_string_lossy()
                ));
            }
        }
    }
    if let Some(overrides) = subagents.get("agentOverrides") {
        let Some(overrides) = overrides.as_object() else {
            return Err(format!(
                "Subagent settings in '{}' have invalid agentOverrides; expected an object keyed by agent.",
                path.to_string_lossy()
            ));
        };
        for (name, value) in overrides {
            let Some(entry) = value.as_object() else {
                return Err(format!(
                    "Subagent settings in '{}' have invalid agentOverrides entry '{name}'; expected a JSON object.",
                    path.to_string_lossy()
                ));
            };
            let parsed_entry =
                parse_override_entry(path, &format!("agentOverrides.{name}"), entry)?;
            if parsed_entry != AgentOverride::default() {
                parsed.overrides.insert(name.clone(), parsed_entry);
            }
        }
    }
    if let Some(by_provider) = subagents.get("agentOverridesByProvider") {
        let Some(by_provider) = by_provider.as_object() else {
            return Err(format!(
                "Subagent settings in '{}' have invalid agentOverridesByProvider; expected an object keyed by provider.",
                path.to_string_lossy()
            ));
        };
        for (provider, value) in by_provider {
            let Some(entries) = value.as_object() else {
                return Err(format!(
                    "Subagent settings in '{}' have invalid agentOverridesByProvider.{provider}; expected an object keyed by agent.",
                    path.to_string_lossy()
                ));
            };
            let mut parsed_entries = BTreeMap::new();
            for (agent_name, value) in entries {
                let Some(entry) = value.as_object() else {
                    return Err(format!(
                        "Subagent settings in '{}' have invalid agentOverridesByProvider.{provider}.{agent_name}; expected a JSON object.",
                        path.to_string_lossy()
                    ));
                };
                let parsed_entry = parse_override_entry(
                    path,
                    &format!("agentOverridesByProvider.{provider}.{agent_name}"),
                    entry,
                )?;
                if parsed_entry != AgentOverride::default() {
                    parsed_entries.insert(agent_name.clone(), parsed_entry);
                }
            }
            parsed
                .provider_overrides
                .insert(provider.clone(), parsed_entries);
        }
    }
    Ok(parsed)
}

fn parse_override_entry(
    path: &std::path::Path,
    name: &str,
    entry: &serde_json::Map<String, Value>,
) -> Result<AgentOverride, String> {
    let mut parsed_entry = AgentOverride::default();
    let mut ignored = Vec::new();
    for (key, field) in entry {
        if !BUILTIN_OVERRIDE_FIELDS.contains(&key.as_str()) {
            let suggestion =
                crate::agents::edit_distance::closest_match(key, BUILTIN_OVERRIDE_FIELDS);
            return Err(format!(
                "Subagent settings in '{}' have invalid {name}: unknown field '{key}'.{}",
                path.to_string_lossy(),
                suggestion
                    .map(|suggestion| format!(" Did you mean '{suggestion}'?"))
                    .unwrap_or_default()
            ));
        }
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
                        ));
                    }
                });
            }
            "disabled" => {
                parsed_entry.disabled =
                    Some(field.as_bool().ok_or_else(|| {
                        invalid_override_field(path, name, "disabled", "a boolean")
                    })?);
            }
            // #2534: `advertise` accepts only booleans (agents.ts:1014).
            "advertise" => {
                parsed_entry.advertise =
                    Some(field.as_bool().ok_or_else(|| {
                        invalid_override_field(path, name, "advertise", "a boolean")
                    })?);
            }
            "tools" => {
                // `parseToolsOverride` (agents.ts:946-951 @ 0fc0eebb):
                // string "inherit" (after trim), boolean false, or a
                // string array; anything else is a settings error.
                parsed_entry.tools = Some(match field {
                    Value::String(value) if value.trim() == "inherit" => ToolsOverride::Inherit,
                    Value::Bool(false) => ToolsOverride::Disable,
                    Value::Array(items) => {
                        let mut names = Vec::new();
                        for item in items {
                            let tool = item.as_str().ok_or_else(|| {
                                invalid_override_field(
                                    path,
                                    name,
                                    "tools",
                                    "an array of strings, \"inherit\", or false",
                                )
                            })?;
                            if tool.starts_with('+') || tool.starts_with('-') {
                                return Err(invalid_override_field(
                                    path,
                                    name,
                                    "tools",
                                    "tool names without '+name'/'-name' modifiers (use tools/excludeTools)",
                                ));
                            }
                            names.push(tool.to_string());
                        }
                        ToolsOverride::List(names)
                    }
                    _ => {
                        return Err(invalid_override_field(
                            path,
                            name,
                            "tools",
                            "an array of strings, \"inherit\", or false",
                        ));
                    }
                });
            }
            "excludeTools" => {
                // #1776: same shape family as other list overrides
                // (`parseOverrideStringArrayOrFalse`, agents.ts:1098).
                parsed_entry.exclude_tools = Some(parse_override_string_array_or_false(
                    field,
                    path,
                    name,
                    "excludeTools",
                )?);
            }
            "allowedAgents" => {
                parsed_entry.allowed_agents = Some(parse_override_string_array_or_false(
                    field,
                    path,
                    name,
                    "allowedAgents",
                )?);
            }
            "thinking" => {
                // #3064: a known level (off..max) or false; a typo used
                // to be appended to the child's model id.
                parsed_entry.thinking = Some(match field {
                    Value::Bool(false) => None,
                    Value::String(level)
                        if crate::launch::model::THINKING_LEVELS.contains(&level.as_str()) =>
                    {
                        Some(level.clone())
                    }
                    _ => {
                        return Err(invalid_override_field(
                            path,
                            name,
                            "thinking",
                            "a known level (off, minimal, low, medium, high, xhigh, max) or false",
                        ));
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
                        ));
                    }
                });
            }
            "inheritProjectContext" => {
                parsed_entry.inherit_project_context = Some(field.as_bool().ok_or_else(|| {
                    invalid_override_field(path, name, "inheritProjectContext", "a boolean")
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
                        ));
                    }
                });
            }
            "acceptanceRole" => {
                parsed_entry.acceptance_role = Some(match field {
                    Value::Bool(false) => None,
                    Value::String(s) if s == "read-only" || s == "writer" => Some(s.clone()),
                    _ => {
                        return Err(invalid_override_field(
                            path,
                            name,
                            "acceptanceRole",
                            "'read-only', 'writer', or false",
                        ));
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
            "defaultProvider" => {
                // #1393 (agents.ts:1087-1090): non-empty string or false.
                parsed_entry.default_provider = Some(match field {
                    Value::Bool(false) => None,
                    Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                    _ => {
                        return Err(invalid_override_field(
                            path,
                            name,
                            "defaultProvider",
                            "a non-empty string or false",
                        ));
                    }
                });
            }
            "allowNestedSubagents" => {
                // #1587 (agents.ts:1100-1102).
                parsed_entry.allow_nested_subagents = Some(field.as_bool().ok_or_else(|| {
                    invalid_override_field(path, name, "allowNestedSubagents", "a boolean")
                })?);
            }
            "outputMode" => {
                // #1305 (agents.ts:979-983): inline | file-only.
                match field.as_str() {
                    Some("inline") | Some("file-only") => {
                        parsed_entry.output_mode =
                            Some(field.as_str().unwrap_or("inline").to_string());
                    }
                    _ => {
                        return Err(invalid_override_field(
                            path,
                            name,
                            "outputMode",
                            "'inline' or 'file-only'",
                        ));
                    }
                }
            }
            "skills" => {
                parsed_entry.skills = Some(parse_override_string_array_or_false(
                    field, path, name, "skills",
                )?);
            }
            "mutationTools" => {
                parsed_entry.mutation_tools = Some(parse_override_string_array_or_false(
                    field,
                    path,
                    name,
                    "mutationTools",
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
    Ok(parsed_entry)
}

fn invalid_override_field(
    path: &std::path::Path,
    name: &str,
    field: &str,
    expected: &str,
) -> String {
    format!(
        "Subagent settings in '{}' have invalid {name}.{field}; expected {expected}.",
        path.to_string_lossy()
    )
}

/// Every field an `agentOverrides` entry may set (`BUILTIN_OVERRIDE_FIELDS`,
/// agents.ts:122-130 @ 3a63e0ad). Fields rpi does not implement stay
/// known-but-ignored (TE-D15); a field outside this list is a typo and fails
/// the load with the nearest known name (#3064).
const BUILTIN_OVERRIDE_FIELDS: [&str; 27] = [
    "description",
    "advertise",
    "machine",
    "output",
    "outputMode",
    "defaultReads",
    "model",
    "defaultProvider",
    "fast",
    "thinking",
    "systemPromptMode",
    "inheritProjectContext",
    "inheritGlobalContext",
    "inheritSkills",
    "defaultContext",
    "acceptanceRole",
    "disabled",
    "systemPrompt",
    "skills",
    "tools",
    "excludeTools",
    "allowNestedSubagents",
    "allowedAgents",
    "extensions",
    "subagentOnlyExtensions",
    "mutationTools",
    "toolBudget",
];

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
/// throws); rpi matches that for `subagents.*` (R18.9.4 BREAKING). The
/// extension's own `config.json` keeps the upstream config.ts warn+empty
/// relaxation (TE-D15), which is a different file.
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
    /// Merged `defaultSubagentOnlyExtensions` (#2322); an explicitly present
    /// project value wins over the user value (resolves independently from
    /// `defaultExtensions`).
    pub default_subagent_only_extensions: Option<Vec<String>>,
    /// Resolved agent-exclusion roots (#2146): user + project entries, each
    /// resolved against its own settings file's directory (`~` and absolute
    /// entries honored); empty = no exclusions.
    pub agent_exclude_roots: Vec<std::path::PathBuf>,
    /// `modelScope` — project wins when a project settings file exists
    /// (same mask discipline as the other defaults).
    pub model_scope: Option<crate::launch::model::ModelScopeConfig>,
    /// `resolveSubagentMaxThinking` (agents.ts:1303-1309): project wins.
    pub max_thinking: Option<String>,
    /// `resolveSubagentDefaultProvider` (agents.ts:1242-1250): project wins.
    pub default_provider: Option<String>,
}

impl SettingsPair {
    /// Apply the selected provider's `agentOverridesByProvider` entries to the
    /// base overrides at both scopes (agents.ts `selectProviderOverrides`).
    pub fn with_provider(&self, provider: Option<&str>) -> SettingsPair {
        let Some(provider) = provider else {
            return self.clone();
        };
        let mut out = self.clone();
        out.user = self.user.with_provider_overrides(Some(provider));
        out.project = self.project.with_provider_overrides(Some(provider));
        out
    }
}

/// Trust-aware settings pair (#2805): an untrusted project contributes no
/// settings, session dir or exclusion roots, so a malformed or hostile
/// project file cannot change discovery or launches. The user level is
/// unchanged.
pub fn read_settings_pair_trusted(
    cwd: &std::path::Path,
    project_trusted: bool,
) -> Result<SettingsPair, String> {
    let user_path = paths::get_agent_dir().join("settings.json");
    let user = read_subagent_settings(&user_path)?;
    let project_path = project_trusted
        .then(|| crate::agents::project_settings_path(cwd))
        .flatten();
    let project = match &project_path {
        Some(path) => read_subagent_settings(path)?,
        None => SubagentSettings::default(),
    };
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
    let default_subagent_only_extensions =
        if project_settings_present && project.default_subagent_only_extensions.is_some() {
            project.default_subagent_only_extensions.clone()
        } else {
            user.default_subagent_only_extensions.clone()
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
    // `resolveSubagentMaxThinking` / `resolveSubagentDefaultProvider`
    // (agents.ts:1303-1309/1242-1250): a project settings file that sets the
    // key masks the user level.
    let max_thinking = if project_settings_present && project.max_thinking.is_some() {
        project.max_thinking.clone()
    } else {
        user.max_thinking.clone()
    };
    let default_provider = if project_settings_present && project.default_provider.is_some() {
        project.default_provider.clone()
    } else {
        user.default_provider.clone()
    };
    // #2146: exclusion roots — user entries resolve against the user agent
    // dir, project entries against the project settings dir; `~` expands and
    // absolute entries pass through.
    let mut agent_exclude_roots: Vec<std::path::PathBuf> = Vec::new();
    for (base, entries) in [
        (user_path.parent(), user.agent_exclude_dirs.as_deref()),
        (
            project_path.as_deref().and_then(std::path::Path::parent),
            project.agent_exclude_dirs.as_deref(),
        ),
    ] {
        let Some(base) = base else { continue };
        let Some(entries) = entries else { continue };
        for entry in entries {
            let expanded = if entry.starts_with('~') || std::path::Path::new(entry).is_absolute() {
                paths::expand_tilde_and_resolve(entry)
            } else {
                base.join(entry)
            };
            if !agent_exclude_roots.contains(&expanded) {
                agent_exclude_roots.push(expanded);
            }
        }
    }
    Ok(SettingsPair {
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
        default_subagent_only_extensions,
        agent_exclude_roots,
        model_scope,
        max_thinking,
        default_provider,
        user,
        project,
    })
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
        assert!(config.slash_commands_enabled());
    }

    #[test]
    fn enable_slash_commands_defaults_true_and_validates_type() {
        assert!(parse("{}").unwrap().slash_commands_enabled());
        assert!(
            !parse(r#"{"enableSlashCommands":false}"#)
                .unwrap()
                .slash_commands_enabled()
        );
        assert!(parse(r#"{"enableSlashCommands":"no"}"#).is_err());
    }

    /// rpi#30: the run-wide child cap resolves with the upstream default 20
    /// (DEFAULT_GLOBAL_CONCURRENCY_LIMIT) and honors explicit values; the
    /// caller (`run_parallel_async`) clamps to ≥ 1 permit.
    #[test]
    fn global_concurrency_limit_defaults_to_20_and_honors_values() {
        assert_eq!(parse("{}").unwrap().global_concurrency_limit(), 20);
        assert_eq!(
            parse(r#"{"globalConcurrencyLimit": 3}"#)
                .unwrap()
                .global_concurrency_limit(),
            3
        );
        // Invalid numbers fall back to the default (the semaphore site
        // clamps ≥ 1).
        assert_eq!(
            parse(r#"{"globalConcurrencyLimit": 0}"#)
                .unwrap()
                .global_concurrency_limit(),
            20
        );
        assert_eq!(
            parse(r#"{"globalConcurrencyLimit": "not-a-number"}"#)
                .unwrap()
                .global_concurrency_limit(),
            20
        );
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

    /// TE19 (R7.1.10.1/R7.1.11.1): the v0.66 settings keys parse, and a
    /// project file that sets them masks the user level (project wins).
    #[test]
    fn v066_settings_keys_parse_and_project_wins() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-v066-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let user_path = dir.join("user-settings.json");
        std::fs::write(
            &user_path,
            r#"{"subagents":{
                "maxThinking":"high",
                "defaultProvider":"user-prov",
                "agentScanDirs":["/tmp/scan-user"]
            }}"#,
        )
        .unwrap();
        let user = read_subagent_settings(&user_path).unwrap();
        assert_eq!(user.max_thinking.as_deref(), Some("high"));
        assert_eq!(user.default_provider.as_deref(), Some("user-prov"));
        assert_eq!(
            user.agent_scan_dirs.as_deref(),
            Some(["/tmp/scan-user".to_string()].as_slice())
        );
        // Invalid vocabulary fails the whole file (agents.ts:1164-1170).
        let bad = dir.join("bad-thinking.json");
        std::fs::write(&bad, r#"{"subagents":{"maxThinking":"ultra"}}"#).unwrap();
        let error = read_subagent_settings(&bad).unwrap_err();
        assert!(error.contains("invalid 'maxThinking'"), "{error}");
        assert!(
            error.contains("off, minimal, low, medium, high, xhigh, or max"),
            "{error}"
        );
        let bad_provider = dir.join("bad-provider.json");
        std::fs::write(&bad_provider, r#"{"subagents":{"defaultProvider":42}}"#).unwrap();
        assert!(
            read_subagent_settings(&bad_provider)
                .unwrap_err()
                .contains("invalid 'defaultProvider'")
        );
        let bad_scan = dir.join("bad-scan.json");
        std::fs::write(&bad_scan, r#"{"subagents":{"agentScanDirs":["/ok",42]}}"#).unwrap();
        assert!(
            read_subagent_settings(&bad_scan)
                .unwrap_err()
                .contains("invalid 'agentScanDirs'")
        );
        // Override fields: defaultProvider string|false, allowNestedSubagents
        // bool, outputMode enum.
        let override_path = dir.join("overrides.json");
        std::fs::write(
            &override_path,
            r#"{"subagents":{"agentOverrides":{
                "scout":{"defaultProvider":"openai","allowNestedSubagents":true,"outputMode":"file-only","advertise":true},
                "worker":{"defaultProvider":false,"outputMode":"inline","advertise":false}
            }}}"#,
        )
        .unwrap();
        let parsed = read_subagent_settings(&override_path).unwrap();
        assert_eq!(
            parsed.overrides["scout"].default_provider,
            Some(Some("openai".to_string()))
        );
        assert_eq!(parsed.overrides["scout"].allow_nested_subagents, Some(true));
        assert_eq!(
            parsed.overrides["scout"].output_mode.as_deref(),
            Some("file-only")
        );
        // #2534: `advertise` is a boolean override (true advertises, false
        // removes the catalog entry; absent leaves the definition untouched).
        assert_eq!(parsed.overrides["scout"].advertise, Some(true));
        assert_eq!(parsed.overrides["worker"].advertise, Some(false));
        // `false` clears the provider (Some(None)).
        assert_eq!(parsed.overrides["worker"].default_provider, Some(None));
        let bad_advertise = dir.join("bad-advertise.json");
        std::fs::write(
            &bad_advertise,
            r#"{"subagents":{"agentOverrides":{"x":{"advertise":"yes"}}}}"#,
        )
        .unwrap();
        assert!(
            read_subagent_settings(&bad_advertise)
                .unwrap_err()
                .contains("advertise")
        );
        let bad_mode = dir.join("bad-mode.json");
        std::fs::write(
            &bad_mode,
            r#"{"subagents":{"agentOverrides":{"x":{"outputMode":"boxed"}}}}"#,
        )
        .unwrap();
        assert!(
            read_subagent_settings(&bad_mode)
                .unwrap_err()
                .contains("outputMode")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1591: waitTool.defaultTimeoutMs parses with validation, and
    /// resolves through env overrides.
    #[test]
    fn control_config_parses_and_resolves() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"control":{"enabled":false,"needsAttentionAfterMs":1000,"activeNoticeAfterTurns":2,"notifyChannels":["event"]}}"#,
        )
        .unwrap();
        let raw = read_json_file(&path).unwrap();
        let config = parse_config(&raw, "config.json").unwrap();
        let resolved = crate::runner::control::resolve(config.control.as_ref(), None);
        assert!(!resolved.enabled);
        assert_eq!(resolved.needs_attention_after_ms, 1_000);
        assert!(resolved.needs_attention_after_ms_is_explicit);
        assert_eq!(resolved.active_notice_after_turns, Some(2));
        assert_eq!(
            resolved.notify_channels,
            vec![crate::runner::control::ControlNotificationChannel::Event]
        );
        std::fs::write(&path, r#"{"control":"nope"}"#).unwrap();
        assert!(parse_config(&read_json_file(&path).unwrap(), "config.json").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_window_config() {
        fn load_config_at(path: &std::path::Path) -> ExtensionConfig {
            let raw = read_json_file(path).expect("config file");
            parse_config(&raw, &path.to_string_lossy()).expect("config parses")
        }
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rpi-extension.json");
        std::fs::write(
            &path,
            r#"{"waitTool":{"enabled":true,"defaultTimeoutMs":120000}}"#,
        )
        .unwrap();
        let config = load_config_at(&path);
        assert_eq!(config.wait_tool_default_timeout_ms(), Some(120_000));
        // Invalid shapes fail the config load.
        let bad = dir.join("bad.json");
        std::fs::write(&bad, r#"{"waitTool":{"defaultTimeoutMs":0}}"#).unwrap();
        assert!(parse_config(&read_json_file(&bad).unwrap(), "bad.json").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_provider_overrides_and_mutation_tools() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-provider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"subagents":{
                "agentOverrides":{"researcher":{"model":"base/model","thinking":"low"}},
                "agentOverridesByProvider":{
                    "anthropic":{"researcher":{"model":"anthropic/model","tools":["read","bash"],"mutationTools":["custom"]}},
                    "openai":{"researcher":{"disabled":true}}
                }
            }}"#,
        )
        .unwrap();
        let settings = read_subagent_settings(&path).unwrap();
        let base = settings.with_provider_overrides(None);
        assert_eq!(
            base.overrides
                .get("researcher")
                .unwrap()
                .model
                .as_ref()
                .and_then(Option::as_deref),
            Some("base/model")
        );
        let anthropic = settings.with_provider_overrides(Some("anthropic"));
        let entry = anthropic.overrides.get("researcher").unwrap();
        assert_eq!(
            entry.model.as_ref().and_then(Option::as_deref),
            Some("anthropic/model")
        );
        assert_eq!(
            entry.thinking,
            Some(Some("low".to_string())),
            "provider merge is field-wise"
        );
        assert_eq!(entry.mutation_tools, Some(Some(vec!["custom".to_string()])));
        assert_eq!(
            entry.tools,
            Some(ToolsOverride::List(vec![
                "read".to_string(),
                "bash".to_string()
            ]))
        );
        let openai = settings.with_provider_overrides(Some("openai"));
        assert_eq!(
            openai.overrides.get("researcher").unwrap().disabled,
            Some(true)
        );
        // Provider-level validation uses the same unknown-field/thinking rules.
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverridesByProvider":{"anthropic":{"x":{"modle":"m"}}}}}"#,
        )
        .unwrap();
        let error = read_subagent_settings(&path).unwrap_err();
        assert!(
            error.contains("agentOverridesByProvider.anthropic.x")
                && error.contains("Did you mean 'model'?"),
            "{error}"
        );
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverrides":{"x":{"tools":["+read"]}}}}"#,
        )
        .unwrap();
        let error = read_subagent_settings(&path).unwrap_err();
        assert!(error.contains("+name'/'-name"), "{error}");
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverrides":{"empty":{"output":"x"}}}}"#,
        )
        .unwrap();
        let settings = read_subagent_settings(&path).unwrap();
        assert!(
            !settings.overrides.contains_key("empty"),
            "an entry with no implemented field must not mask the other scope"
        );
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverridesByProvider":{"anthropic":{"x":{"thinking":"hgih"}}}}}"#,
        )
        .unwrap();
        assert!(read_subagent_settings(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_settings_file_fails_fast() {
        let dir =
            std::env::temp_dir().join(format!("rpi-sub-cfg-malformed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{not json").unwrap();
        let error = read_subagent_settings(&path).expect_err("malformed JSON must fail");
        assert!(error.contains("Failed to parse settings file"), "{error}");
        std::fs::write(&path, r#"{"subagents":"nope"}"#).unwrap();
        assert!(read_subagent_settings(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_pair_fails_fast_but_ignores_untrusted_project() {
        let root = std::env::temp_dir().join(format!("rpi-sub-cfg-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".rpi")).unwrap();
        std::fs::write(
            project.join(".rpi").join("settings.json"),
            r#"{"subagents":{"agentOverrides":{"x":{"modle":"m"}}}}"#,
        )
        .unwrap();
        assert!(
            read_settings_pair_trusted(&project, true).is_err(),
            "trusted project parse errors fail the load"
        );
        assert!(
            read_settings_pair_trusted(&project, false).is_ok(),
            "an untrusted project is not read"
        );
        let _ = std::fs::remove_dir_all(&root);
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
                    "worker":{"systemPromptMode":"append",
                              "thinking":false,"defaultContext":"fork","acceptanceRole":"writer",
                              "skills":["s1"],"systemPrompt":"custom","inheritSkills":false},
                    "reviewer":{"toolBudget":false}
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
            Some(ToolsOverride::List(vec![
                "read".to_string(),
                "write".to_string()
            ]))
        );
        assert_eq!(settings.overrides.get("scout").unwrap().model, Some(None));
        let worker = settings.overrides.get("worker").unwrap();
        assert_eq!(worker.system_prompt_mode.as_deref(), Some("append"));
        assert_eq!(worker.thinking, Some(None));
        assert_eq!(worker.default_context, Some(Some("fork".to_string())));
        assert_eq!(worker.acceptance_role, Some(Some("writer".to_string())));
        assert_eq!(worker.skills, Some(Some(vec!["s1".to_string()])));
        assert_eq!(worker.system_prompt.as_deref(), Some("custom"));
        assert_eq!(worker.inherit_skills, Some(false));
        // toolBudget is a known upstream field rpi does not implement →
        // ignored; an entry with no implemented field is not inserted.
        assert!(!settings.overrides.contains_key("reviewer"));
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
        // #3064: a misspelled field fails the load with the nearest known name.
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverrides":{"researcher":{"modle":"x"}}}}"#,
        )
        .unwrap();
        let error = read_subagent_settings(&path).unwrap_err();
        assert!(
            error.contains("unknown field 'modle'") && error.contains("Did you mean 'model'?"),
            "{error}"
        );
        // #3064: thinking must be a known level or false.
        std::fs::write(
            &path,
            r#"{"subagents":{"agentOverrides":{"researcher":{"thinking":"hgih"}}}}"#,
        )
        .unwrap();
        let error = read_subagent_settings(&path).unwrap_err();
        assert!(
            error.contains("invalid agentOverrides.researcher.thinking"),
            "{error}"
        );
        for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            std::fs::write(
                &path,
                format!(
                    r#"{{"subagents":{{"agentOverrides":{{"researcher":{{"thinking":"{level}"}}}}}}}}"#
                ),
            )
            .unwrap();
            let settings = read_subagent_settings(&path).unwrap();
            assert_eq!(
                settings.overrides.get("researcher").unwrap().thinking,
                Some(Some(level.to_string()))
            );
        }
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
        rpi_test_env::set_var("RPI_SUBAGENT_WAIT_TOOL_ENABLED", "1");
        assert!(config.wait_tool_enabled());
        rpi_test_env::set_var("RPI_SUBAGENT_WAIT_TOOL_ENABLED", "bogus");
        // invalid env values warn and fall through to the config value.
        assert!(!config.wait_tool_enabled());
        rpi_test_env::remove_var("RPI_SUBAGENT_WAIT_TOOL_ENABLED");
    }

    #[test]
    fn untrusted_settings_pair_ignores_the_project_level() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-trust-{}", std::process::id()));
        let project = dir.join("project");
        std::fs::create_dir_all(project.join(".rpi")).unwrap();
        std::fs::create_dir_all(dir.join("user")).unwrap();
        std::fs::write(
            project.join(".rpi").join("settings.json"),
            r#"{"subagents":{"defaultModel":"project/model","defaultThinking":"high"}}"#,
        )
        .unwrap();
        let trusted = read_settings_pair_trusted(&project, true).unwrap();
        assert_eq!(trusted.default_model.as_deref(), Some("project/model"));
        assert_eq!(trusted.default_thinking.as_deref(), Some("high"));
        let untrusted = read_settings_pair_trusted(&project, false).unwrap();
        assert_eq!(untrusted.default_model, None);
        assert_eq!(untrusted.default_thinking, None);
        assert_eq!(untrusted.project, SubagentSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
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

/// TE18 (R7.1.4.1/.2): `tools` override tri-state (`"inherit"` / `false` /
/// array, #1047/#1049 — `parseToolsOverride`, agents.ts:946-951 @ 0fc0eebb)
/// and the `excludeTools` override list (#1776). Ported intents:
/// agent-overrides.test.ts:91/119/133/151.
#[cfg(test)]
mod te18_override_parse_tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn parse_entry(json: &str) -> Result<AgentOverride, String> {
        let nonce = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("rpi-sub-cfg-te18-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            format!("{{\"subagents\":{{\"agentOverrides\":{{\"researcher\":{json}}}}}}}"),
        )
        .unwrap();
        let settings = read_subagent_settings(&path);
        let _ = std::fs::remove_dir_all(&dir);
        settings.map(|settings| settings.overrides["researcher"].clone())
    }

    #[test]
    fn tools_override_tri_state() {
        assert_eq!(
            parse_entry(r#"{"tools":"inherit"}"#).unwrap().tools,
            Some(ToolsOverride::Inherit)
        );
        assert_eq!(
            parse_entry(r#"{"tools":false}"#).unwrap().tools,
            Some(ToolsOverride::Disable)
        );
        assert_eq!(
            parse_entry(r#"{"tools":["read","write"]}"#).unwrap().tools,
            Some(ToolsOverride::List(vec![
                "read".to_string(),
                "write".to_string()
            ]))
        );
        // agent-overrides.test.ts:151 — a bare string tool override is a
        // settings error naming the file/agent/field.
        let error = parse_entry(r#"{"tools":"read"}"#).unwrap_err();
        assert!(error.contains("agentOverrides.researcher.tools"), "{error}");
        assert!(
            error.contains("an array of strings, \"inherit\", or false"),
            "{error}"
        );
    }

    #[test]
    fn exclude_tools_override_shapes() {
        assert_eq!(
            parse_entry(r#"{"excludeTools":["write","unknown_tool"]}"#)
                .unwrap()
                .exclude_tools,
            Some(Some(vec!["write".to_string(), "unknown_tool".to_string()]))
        );
        // `false` clears back to undeclared.
        assert_eq!(
            parse_entry(r#"{"excludeTools":false}"#)
                .unwrap()
                .exclude_tools,
            Some(None)
        );
        // Non-string entries are settings errors.
        assert!(parse_entry(r#"{"excludeTools":[1]}"#).is_err());
    }
    #[test]
    fn default_subagent_only_extensions_parse_and_merge() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-dsoe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.json");
        std::fs::write(
            &user,
            r#"{"subagents":{"defaultSubagentOnlyExtensions":["/x/ext.so"]}}"#,
        )
        .unwrap();
        let settings = read_subagent_settings(&user).unwrap();
        assert_eq!(
            settings.default_subagent_only_extensions,
            Some(vec!["/x/ext.so".to_string()])
        );
        // Non-array / blank entries fail the load with the key named.
        let bad = dir.join("bad.json");
        std::fs::write(
            &bad,
            r#"{"subagents":{"defaultSubagentOnlyExtensions":"x"}}"#,
        )
        .unwrap();
        let error = read_subagent_settings(&bad).unwrap_err();
        assert!(error.contains("defaultSubagentOnlyExtensions"), "{error}");
        let blank = dir.join("blank.json");
        std::fs::write(
            &blank,
            r#"{"subagents":{"defaultSubagentOnlyExtensions":["  "]}}"#,
        )
        .unwrap();
        assert!(read_subagent_settings(&blank).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn agent_exclude_dirs_parse() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-cfg-excl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"subagents":{"agentExcludeDirs":["nested/vendor"]}}"#,
        )
        .unwrap();
        let settings = read_subagent_settings(&path).unwrap();
        assert_eq!(
            settings.agent_exclude_dirs,
            Some(vec!["nested/vendor".to_string()])
        );
        let bad = dir.join("bad.json");
        std::fs::write(&bad, r#"{"subagents":{"agentExcludeDirs":["  "]}}"#).unwrap();
        let error = read_subagent_settings(&bad).unwrap_err();
        assert!(error.contains("agentExcludeDirs"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
