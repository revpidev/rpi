//! Port of `packages/coding-agent/src/core/settings-manager.ts` (and the
//! `parseHttpIdleTimeoutMs` subset of `core/http-dispatcher.ts`)
//! @ pi 0.82.1 (2efa728).
//!
//! Settings are stored as an insertion-ordered JSON object
//! ([`serde_json::Map`] with `preserve_order`), the counterpart of the JS
//! object upstream casts to `Settings`: unknown keys round-trip verbatim and
//! `JSON.stringify(obj, null, 2)` key order is preserved. Typed getters apply
//! the upstream per-getter defaults; nothing is defaulted on disk.
//!
//! Intentional differences:
//! - Synchronous API: upstream serializes writes through a `Promise` write
//!   queue (`enqueueWrite`); here every setter persists inline and
//!   [`SettingsManager::flush`] is a no-op kept for API parity. Sync
//!   `std::fs` I/O mirrors the upstream `*Sync` calls — async callers must
//!   wrap calls in `tokio::task::spawn_blocking` (session_manager.rs
//!   precedent).
//! - Cross-process locking uses `fs2` flock on the settings file itself
//!   (coding-standards §9.2 pins fs2) instead of proper-lockfile's
//!   `<file>.lock` sidecar. The acquisition shape is preserved: lock only
//!   when the file exists or a write is about to happen, retry 10× with a
//!   20ms delay, contention-only retry (`ELOCKED` ↔ `WouldBlock`). When the
//!   file does not exist yet, the lock open creates it empty right before
//!   the write (proper-lockfile needs no target file); the content written
//!   immediately after is identical.
//! - Errors surface as `Result<_, PirError>` instead of thrown exceptions;
//!   per-scope error collection (`drainErrors`) is preserved.
//! - `randomUUID()` for the analytics `trackingId` reads `/dev/urandom`
//!   (unix) with a time/pid/counter-mix fallback — no `rand`/`uuid` crate in
//!   the dependency baseline (appendix A). The fallback is not
//!   cryptographically secure; collision resistance is what matters here.
//! - Wrongly-typed values in settings.json (e.g. `steeringMode: 5`) fall back
//!   to defaults in the typed getters instead of passing through raw as in
//!   JS; valid files behave identically.
//! - `parseHttpIdleTimeoutMs` number parsing uses Rust `f64` parsing; JS
//!   `Number()` exotica (`"0x10"`, `"Infinity"`) parse differently. All
//!   realistic values (decimal integers, `"disabled"`, `""`) match.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use pir_agent::types::{QueueMode, ThinkingLevel};
use pir_ai::types::Transport;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::environment;
use crate::error::PirError;
use crate::tools::path_utils::{normalize_path, resolve_path};

/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS` (http-dispatcher.ts:4).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;

// ===========================================================================
// Settings value model (settings-manager.ts:11-129)
// ===========================================================================

/// `CompactionSettings` (settings-manager.ts:11-15) — file shape; all fields
/// optional, defaults live in the getters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    /// default: true
    pub enabled: Option<bool>,
    /// default: 16384
    pub reserve_tokens: Option<u64>,
    /// default: 20000
    pub keep_recent_tokens: Option<u64>,
}

/// `BranchSummarySettings` (settings-manager.ts:17-20) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BranchSummarySettings {
    /// default: 16384 (tokens reserved for prompt + LLM response)
    pub reserve_tokens: Option<u64>,
    /// default: false — when true, skips "Summarize branch?" prompt and
    /// defaults to no summary
    pub skip_prompt: Option<bool>,
}

/// `ProviderRetrySettings` (settings-manager.ts:22-26) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderRetrySettings {
    /// SDK/provider request timeout in milliseconds
    pub timeout_ms: Option<u64>,
    /// SDK/provider retry attempts
    pub max_retries: Option<u64>,
    /// default: 60000 (max server-requested delay before failing)
    pub max_retry_delay_ms: Option<u64>,
}

/// `RetrySettings` (settings-manager.ts:28-33) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
    /// default: true
    pub enabled: Option<bool>,
    /// default: 3
    pub max_retries: Option<u64>,
    /// default: 2000 (exponential backoff: 2s, 4s, 8s)
    pub base_delay_ms: Option<u64>,
    pub provider: Option<ProviderRetrySettings>,
}

/// `TerminalSettings` (settings-manager.ts:35-40) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalSettings {
    /// default: true (only relevant if terminal supports images)
    pub show_images: Option<bool>,
    /// default: 60 (preferred inline image width in terminal cells)
    pub image_width_cells: Option<u64>,
    /// default: false (clear empty rows when content shrinks)
    pub clear_on_shrink: Option<bool>,
    /// default: false (OSC 9;4 terminal progress indicators)
    pub show_terminal_progress: Option<bool>,
}

/// `ImageSettings` (settings-manager.ts:42-45) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageSettings {
    /// default: true (resize images to 2000x2000 max for better model
    /// compatibility)
    pub auto_resize: Option<bool>,
    /// default: false — when true, prevents all images from being sent to
    /// LLM providers
    pub block_images: Option<bool>,
}

/// `ThinkingBudgetsSettings` (settings-manager.ts:47-52) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThinkingBudgetsSettings {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

/// `MarkdownSettings` (settings-manager.ts:54-56) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarkdownSettings {
    /// default: "  "
    pub code_block_indent: Option<String>,
}

/// `WarningSettings` (settings-manager.ts:58-60) — file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WarningSettings {
    /// default: true
    pub anthropic_extra_usage: Option<bool>,
}

/// `DefaultProjectTrust = "ask" | "always" | "never"` (settings-manager.ts:62).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefaultProjectTrust {
    #[serde(rename = "ask")]
    Ask,
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "never")]
    Never,
}

/// `TransportSetting = Transport` (settings-manager.ts:64).
pub type TransportSetting = Transport;

/// `settings.doubleEscapeAction`: `"fork" | "tree" | "none"`
/// (settings-manager.ts:116).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DoubleEscapeAction {
    #[serde(rename = "fork")]
    Fork,
    #[serde(rename = "tree")]
    Tree,
    #[serde(rename = "none")]
    None,
}

/// `settings.treeFilterMode`: `"default" | "no-tools" | "user-only" |
/// "labeled-only" | "all"` (settings-manager.ts:117).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeFilterMode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "no-tools")]
    NoTools,
    #[serde(rename = "user-only")]
    UserOnly,
    #[serde(rename = "labeled-only")]
    LabeledOnly,
    #[serde(rename = "all")]
    All,
}

/// `PackageSource` (settings-manager.ts:72-81): string form loads all
/// resources from the package; object form filters which resources to load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    /// Load all resources from the package.
    Source(String),
    /// Filter which resources to load.
    Filtered(PackageSourceFilter),
}

/// Object form of [`PackageSource`] (settings-manager.ts:74-80).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PackageSourceFilter {
    pub source: String,
    /// autoload=false: start empty and only apply explicit resource patterns
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
}

/// `Settings` (settings-manager.ts:83-129).
///
/// Stored as the raw JSON object (insertion-ordered) rather than a typed
/// struct: upstream casts parsed JSON to `Settings`, so unknown keys survive
/// loads, merges, and writes. Typed access goes through the
/// [`SettingsManager`] getters, which apply the upstream defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Settings {
    fields: Map<String, Value>,
}

impl Settings {
    /// Empty settings (`{}`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a raw JSON object.
    pub fn from_map(fields: Map<String, Value>) -> Self {
        Settings { fields }
    }

    /// The raw JSON object (insertion-ordered).
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.fields
    }

    /// Consume into the raw JSON object.
    pub fn into_map(self) -> Map<String, Value> {
        self.fields
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    fn set(&mut self, key: &str, value: Value) {
        self.fields.insert(key.to_string(), value);
    }

    fn remove(&mut self, key: &str) {
        self.fields.shift_remove(key);
    }

    /// `settings[key]?.[sub]` — only when `settings[key]` is an object.
    fn nested(&self, key: &str, sub: &str) -> Option<&Value> {
        self.fields.get(key)?.as_object()?.get(sub)
    }

    /// `if (!settings[key]) settings[key] = {}; settings[key][sub] = value`
    /// (e.g. settings-manager.ts:765-768). A non-object `settings[key]` is
    /// replaced by `{}` (upstream would throw a TypeError on such malformed
    /// data; replacing is the closest recoverable behavior).
    fn set_nested(&mut self, key: &str, sub: &str, value: Value) {
        let entry = self
            .fields
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        if let Value::Object(map) = entry {
            map.insert(sub.to_string(), value);
        }
    }

    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    fn nested_bool(&self, key: &str, sub: &str) -> Option<bool> {
        self.nested(key, sub)?.as_bool()
    }
}

/// JS number → non-negative integer (`Math.floor` semantics, `as u64`
/// saturates negatives to 0 — see the module-level wrong-type deviation).
fn value_u64(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| n.floor() as u64)
}

/// Serialize an infallibly-serializable value (local enums, `PackageSource`).
fn json_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// deepMergeSettings (settings-manager.ts:132-160)
// ---------------------------------------------------------------------------

/// `deepMergeSettings(base, overrides)` — the upstream comment claims
/// recursion but the code performs a **single-level shallow merge** for
/// nested objects: top-level keys take the union; when both values at one
/// top-level key are plain objects the result is `{...base, ...override}`
/// (so depth ≥ 2 nesting is replaced wholesale); primitives, arrays, and
/// `null` always replace wholesale (requirements §7.7).
///
/// Key order matches JS spread semantics: overridden keys keep their base
/// position, new keys append (indexmap insertion semantics).
fn deep_merge_settings(base: &Settings, overrides: &Settings) -> Settings {
    let mut result = base.fields.clone();
    for (key, override_value) in &overrides.fields {
        // JS `undefined` override values are skipped; JSON has no undefined —
        // absent keys simply never appear in `overrides`.
        if let (Value::Object(override_obj), Some(Value::Object(base_obj))) =
            (override_value, base.fields.get(key))
        {
            let mut merged = base_obj.clone();
            for (sub_key, sub_value) in override_obj {
                merged.insert(sub_key.clone(), sub_value.clone());
            }
            result.insert(key.clone(), Value::Object(merged));
        } else {
            result.insert(key.clone(), override_value.clone());
        }
    }
    Settings { fields: result }
}

// ---------------------------------------------------------------------------
// Timeout setting parsing (settings-manager.ts:162-171, http-dispatcher.ts:17-33)
// ---------------------------------------------------------------------------

/// `parseHttpIdleTimeoutMs` (http-dispatcher.ts:17-33): `"disabled"` → 0,
/// empty/blank string → `None`, numeric string → parsed number; non-finite
/// or negative → `None`; result is `Math.floor`ed.
fn parse_http_idle_timeout_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            // JS `Number(trimmed)` — Rust f64 parsing for the decimal subset.
            match trimmed.parse::<f64>() {
                Ok(n) => number_to_timeout(n),
                Err(_) => None,
            }
        }
        Value::Number(n) => number_to_timeout(n.as_f64()?),
        _ => None,
    }
}

fn number_to_timeout(n: f64) -> Option<u64> {
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some(n.floor() as u64)
}

/// `String(value)` for the "Invalid … setting: {value}" error message:
/// strings unquoted, everything else in JSON form.
fn js_display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `parseTimeoutSetting` (settings-manager.ts:162-171): a present but
/// unparsable value throws `Invalid {setting_name} setting: {value}`.
fn parse_timeout_setting(
    value: Option<&Value>,
    setting_name: &str,
) -> Result<Option<u64>, PirError> {
    match value {
        None => Ok(None),
        Some(v) => match parse_http_idle_timeout_ms(v) {
            Some(timeout) => Ok(Some(timeout)),
            None => Err(PirError::Settings(format!(
                "Invalid {setting_name} setting: {}",
                js_display(v)
            ))),
        },
    }
}

// ---------------------------------------------------------------------------
// Storage backends (settings-manager.ts:173-272)
// ---------------------------------------------------------------------------

/// `SettingsScope = "global" | "project"` (settings-manager.ts:173).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettingsScope {
    #[serde(rename = "global")]
    Global,
    #[serde(rename = "project")]
    Project,
}

impl SettingsScope {
    /// `"global" | "project"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            SettingsScope::Global => "global",
            SettingsScope::Project => "project",
        }
    }
}

/// `SettingsManagerCreateOptions` (settings-manager.ts:175-177).
#[derive(Debug, Clone, Copy)]
pub struct SettingsManagerCreateOptions {
    /// default: true
    pub project_trusted: bool,
}

impl Default for SettingsManagerCreateOptions {
    fn default() -> Self {
        SettingsManagerCreateOptions {
            project_trusted: true,
        }
    }
}

/// `SettingsError` (settings-manager.ts:183-186).
#[derive(Debug)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub error: PirError,
}

/// Callback signature for [`SettingsStorage::with_lock`]: receives the
/// current file content (`None` when the file does not exist) and returns
/// the content to write, `Ok(None)` for a pure read, or an error to abort
/// without writing.
pub type WithLockCallback<'a> = dyn FnMut(Option<&str>) -> Result<Option<String>, PirError> + 'a;

/// `SettingsStorage` (settings-manager.ts:179-181).
pub trait SettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut WithLockCallback<'_>,
    ) -> Result<(), PirError>;
}

/// proper-lockfile acquisition shape (settings-manager.ts:199-224): 10
/// attempts, 20ms delay, contention-only retry.
const LOCK_MAX_ATTEMPTS: u32 = 10;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// `FileSettingsStorage` (settings-manager.ts:188-255).
pub struct FileSettingsStorage {
    global_settings_path: PathBuf,
    project_settings_path: PathBuf,
}

impl FileSettingsStorage {
    /// `new FileSettingsStorage(cwd, agentDir)` (settings-manager.ts:192-197):
    /// both paths are normalized and resolved against the process cwd when
    /// relative.
    pub fn new(cwd: &Path, agent_dir: &Path) -> Self {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let resolved_cwd = resolve_path(&cwd.to_string_lossy(), &base);
        let resolved_agent_dir = resolve_path(&agent_dir.to_string_lossy(), &base);
        FileSettingsStorage {
            global_settings_path: resolved_agent_dir.join("settings.json"),
            project_settings_path: crate::config::get_project_settings_path(&resolved_cwd),
        }
    }

    /// `{agentDir}/settings.json` (settings-manager.ts:195).
    pub fn global_settings_path(&self) -> &Path {
        &self.global_settings_path
    }

    /// `{cwd}/.pir/settings.json` (settings-manager.ts:196).
    pub fn project_settings_path(&self) -> &Path {
        &self.project_settings_path
    }

    /// `acquireLockSyncWithRetry` (settings-manager.ts:199-224), with fs2
    /// flock in place of proper-lockfile (see module header). Returns the
    /// locked file handle; the flock releases on drop.
    ///
    /// `create` opens with `O_CREAT` for the write path, where upstream
    /// locks before the file exists (proper-lockfile needs no target file).
    fn acquire_lock_with_retry(path: &Path, create: bool) -> Result<std::fs::File, PirError> {
        for attempt in 1..=LOCK_MAX_ATTEMPTS {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true);
            if create {
                options.create(true);
            }
            let file = options.open(path).map_err(PirError::Io)?;
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if attempt == LOCK_MAX_ATTEMPTS {
                        return Err(PirError::Settings(format!(
                            "Failed to acquire settings lock for {}: {error}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(error) => return Err(PirError::Io(error)),
            }
        }
        Err(PirError::Settings(format!(
            "Failed to acquire settings lock for {}",
            path.display()
        )))
    }
}

impl SettingsStorage for FileSettingsStorage {
    /// `withLock` (settings-manager.ts:226-254): the lock is only acquired
    /// when the file exists or a write is about to happen; the directory is
    /// only created when a write actually happens; the write is non-atomic
    /// (`writeFileSync` ↔ `std::fs::write`, no temp file + rename).
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut WithLockCallback<'_>,
    ) -> Result<(), PirError> {
        let path = match scope {
            SettingsScope::Global => &self.global_settings_path,
            SettingsScope::Project => &self.project_settings_path,
        };
        let dir = path.parent().map(Path::to_path_buf);

        let file_exists = path.exists();
        let mut guard: Option<std::fs::File> = None;
        let current = if file_exists {
            guard = Some(Self::acquire_lock_with_retry(path, false)?);
            Some(std::fs::read_to_string(path)?)
        } else {
            None
        };

        let next = f(current.as_deref())?;
        if let Some(next) = next {
            if let Some(dir) = &dir {
                if !dir.exists() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            if guard.is_none() {
                guard = Some(Self::acquire_lock_with_retry(path, true)?);
            }
            std::fs::write(path, next)?;
        }

        // Explicit release mirroring upstream's `finally { release() }`; the
        // flock would also release when the handle drops.
        if let Some(file) = guard {
            let _ = file.unlock();
        }
        Ok(())
    }
}

/// `InMemorySettingsStorage` (settings-manager.ts:257-272) — test backend.
#[derive(Debug, Default)]
pub struct InMemorySettingsStorage {
    global: Option<String>,
    project: Option<String>,
}

impl InMemorySettingsStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current global content, if any.
    pub fn global_content(&self) -> Option<&str> {
        self.global.as_deref()
    }

    /// Current project content, if any.
    pub fn project_content(&self) -> Option<&str> {
        self.project.as_deref()
    }
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut WithLockCallback<'_>,
    ) -> Result<(), PirError> {
        let current = match scope {
            SettingsScope::Global => self.global.as_deref(),
            SettingsScope::Project => self.project.as_deref(),
        };
        let next = f(current)?;
        if let Some(next) = next {
            match scope {
                SettingsScope::Global => self.global = Some(next),
                SettingsScope::Project => self.project = Some(next),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Legacy-format migrations (settings-manager.ts:381-440)
// ---------------------------------------------------------------------------

/// `migrateSettings` (settings-manager.ts:381-440): migrates old settings
/// formats in place. Idempotent — re-runs on every load and on the disk
/// content read back during writes (settings-manager.ts:365,586).
fn migrate_settings(fields: &mut Map<String, Value>) {
    // Migrate queueMode -> steeringMode (settings-manager.ts:382-386).
    if fields.contains_key("queueMode") && !fields.contains_key("steeringMode") {
        if let Some(value) = fields.shift_remove("queueMode") {
            fields.insert("steeringMode".to_string(), value);
        }
    }

    // Migrate legacy websockets boolean -> transport enum
    // (settings-manager.ts:388-392).
    if !fields.contains_key("transport") {
        if let Some(Value::Bool(websockets)) = fields.get("websockets") {
            let websockets = *websockets;
            fields.shift_remove("websockets");
            fields.insert(
                "transport".to_string(),
                Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
            );
        }
    }

    // Migrate old skills object format to new array format
    // (settings-manager.ts:394-413).
    if let Some(Value::Object(skills_settings)) = fields.get("skills") {
        let skills_settings = skills_settings.clone();
        if let Some(enable_skill_commands) = skills_settings.get("enableSkillCommands") {
            if !fields.contains_key("enableSkillCommands") {
                fields.insert(
                    "enableSkillCommands".to_string(),
                    enable_skill_commands.clone(),
                );
            }
        }
        match skills_settings.get("customDirectories") {
            Some(Value::Array(directories)) if !directories.is_empty() => {
                fields.insert("skills".to_string(), Value::Array(directories.clone()));
            }
            _ => {
                fields.shift_remove("skills");
            }
        }
    }

    // Migrate retry.maxDelayMs -> retry.provider.maxRetryDelayMs
    // (settings-manager.ts:415-437).
    if let Some(Value::Object(retry_settings)) = fields.get("retry") {
        let mut retry_settings = retry_settings.clone();
        let provider_settings = retry_settings
            .get("provider")
            .and_then(Value::as_object)
            .cloned();
        if let Some(max_delay_ms @ Value::Number(_)) = retry_settings.get("maxDelayMs") {
            let existing = provider_settings
                .as_ref()
                .and_then(|p| p.get("maxRetryDelayMs"));
            if existing.is_none() || matches!(existing, Some(Value::Null)) {
                let mut provider = provider_settings.unwrap_or_default();
                provider.insert("maxRetryDelayMs".to_string(), max_delay_ms.clone());
                retry_settings.insert("provider".to_string(), Value::Object(provider));
            }
        }
        // Unconditional delete (settings-manager.ts:436).
        retry_settings.shift_remove("maxDelayMs");
        fields.insert("retry".to_string(), Value::Object(retry_settings));
    }
}

// ---------------------------------------------------------------------------
// SettingsManager (settings-manager.ts:274-1234)
// ---------------------------------------------------------------------------

/// `SettingsManager` (settings-manager.ts:274).
pub struct SettingsManager {
    storage: Box<dyn SettingsStorage>,
    global_settings: Settings,
    project_settings: Settings,
    settings: Settings,
    project_trusted: bool,
    /// Track global fields modified during session (settings-manager.ts:280).
    modified_fields: HashSet<String>,
    /// Track global nested field modifications (settings-manager.ts:281).
    modified_nested_fields: HashMap<String, HashSet<String>>,
    /// Track project fields modified during session (settings-manager.ts:282).
    modified_project_fields: HashSet<String>,
    /// Track project nested field modifications (settings-manager.ts:283).
    modified_project_nested_fields: HashMap<String, HashSet<String>>,
    /// Set when the global settings file had parse errors — blocks writes.
    global_settings_load_error: Option<PirError>,
    /// Set when the project settings file had parse errors — blocks writes.
    project_settings_load_error: Option<PirError>,
    errors: Vec<SettingsError>,
}

impl SettingsManager {
    fn new(
        storage: Box<dyn SettingsStorage>,
        initial_global: Settings,
        initial_project: Settings,
        global_load_error: Option<PirError>,
        project_load_error: Option<PirError>,
        initial_errors: Vec<SettingsError>,
        project_trusted: bool,
    ) -> Self {
        let settings = deep_merge_settings(&initial_global, &initial_project);
        SettingsManager {
            storage,
            global_settings: initial_global,
            project_settings: initial_project,
            settings,
            project_trusted,
            modified_fields: HashSet::new(),
            modified_nested_fields: HashMap::new(),
            modified_project_fields: HashSet::new(),
            modified_project_nested_fields: HashMap::new(),
            global_settings_load_error: global_load_error,
            project_settings_load_error: project_load_error,
            errors: initial_errors,
        }
    }

    /// `SettingsManager.create(cwd, agentDir?, options?)`
    /// (settings-manager.ts:309-316): file-backed manager; `agent_dir`
    /// defaults to [`crate::config::get_agent_dir`].
    pub fn create(
        cwd: &Path,
        agent_dir: Option<&Path>,
        options: SettingsManagerCreateOptions,
    ) -> Self {
        let agent_dir = agent_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(crate::config::get_agent_dir);
        Self::from_storage(FileSettingsStorage::new(cwd, &agent_dir), options)
    }

    /// `SettingsManager.fromStorage(storage, options?)`
    /// (settings-manager.ts:319-340).
    pub fn from_storage<S: SettingsStorage + 'static>(
        mut storage: S,
        options: SettingsManagerCreateOptions,
    ) -> Self {
        let project_trusted = options.project_trusted;
        let (global_settings, global_error) =
            Self::try_load_from_storage(&mut storage, SettingsScope::Global, true);
        let (project_settings, project_error) =
            Self::try_load_from_storage(&mut storage, SettingsScope::Project, project_trusted);
        let mut initial_errors = Vec::new();
        if let Some(error) = &global_error {
            initial_errors.push(SettingsError {
                scope: SettingsScope::Global,
                error: PirError::Settings(error.to_string()),
            });
        }
        if let Some(error) = &project_error {
            initial_errors.push(SettingsError {
                scope: SettingsScope::Project,
                error: PirError::Settings(error.to_string()),
            });
        }
        SettingsManager::new(
            Box::new(storage),
            global_settings,
            project_settings,
            global_error,
            project_error,
            initial_errors,
            project_trusted,
        )
    }

    /// `SettingsManager.inMemory(settings?, options?)`
    /// (settings-manager.ts:343-348): no file I/O. The input is migrated and
    /// stored as the global scope.
    pub fn in_memory(settings: Settings, options: SettingsManagerCreateOptions) -> Self {
        let mut storage = InMemorySettingsStorage::new();
        let mut fields = settings.into_map();
        migrate_settings(&mut fields);
        // Serializing a plain JSON map cannot fail; fall back to "{}" rather
        // than panic if it ever does.
        let json = serde_json::to_string_pretty(&Value::Object(fields))
            .unwrap_or_else(|_| "{}".to_string());
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| Ok(Some(json.clone())));
        Self::from_storage(storage, options)
    }

    /// `loadFromStorage` (settings-manager.ts:350-366): untrusted projects
    /// read as `{}`; parse errors propagate to the caller.
    fn load_from_storage(
        storage: &mut dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> Result<Settings, PirError> {
        if scope == SettingsScope::Project && !project_trusted {
            return Ok(Settings::new());
        }

        let mut content: Option<String> = None;
        storage.with_lock(scope, &mut |current| {
            content = current.map(str::to_owned);
            Ok(None)
        })?;

        // `if (!content) return {}` — missing file and empty string both.
        let content = match content {
            Some(content) if !content.is_empty() => content,
            _ => return Ok(Settings::new()),
        };
        let value: Value = serde_json::from_str(&content)?;
        let mut fields = match value {
            Value::Object(map) => map,
            // Upstream throws a TypeError from `migrateSettings` on
            // non-object documents — same "parse error" path.
            _ => {
                return Err(PirError::Settings(
                    "Failed to parse settings: top level is not a JSON object".to_string(),
                ));
            }
        };
        migrate_settings(&mut fields);
        Ok(Settings::from_map(fields))
    }

    /// `tryLoadFromStorage` (settings-manager.ts:368-378): parse failures
    /// yield empty settings plus the error — never partial data.
    fn try_load_from_storage(
        storage: &mut dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> (Settings, Option<PirError>) {
        match Self::load_from_storage(storage, scope, project_trusted) {
            Ok(settings) => (settings, None),
            Err(error) => (Settings::new(), Some(error)),
        }
    }

    /// `getGlobalSettings` (settings-manager.ts:442-444).
    pub fn get_global_settings(&self) -> Settings {
        self.global_settings.clone()
    }

    /// `getProjectSettings` (settings-manager.ts:446-448).
    pub fn get_project_settings(&self) -> Settings {
        self.project_settings.clone()
    }

    /// `isProjectTrusted` (settings-manager.ts:450-452).
    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// `setProjectTrusted` (settings-manager.ts:454-477).
    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }

        self.project_trusted = trusted;
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();

        if !trusted {
            self.project_settings = Settings::new();
            self.project_settings_load_error = None;
            self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
            return;
        }

        let (settings, error) =
            Self::try_load_from_storage(&mut *self.storage, SettingsScope::Project, trusted);
        self.project_settings = settings;
        self.project_settings_load_error = error;
        if let Some(error) = &self.project_settings_load_error {
            self.errors.push(SettingsError {
                scope: SettingsScope::Project,
                error: PirError::Settings(error.to_string()),
            });
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// `reload` (settings-manager.ts:479-505). On load errors the previous
    /// in-memory settings are kept and the error is recorded; modified-field
    /// tracking is cleared either way.
    ///
    /// Synchronous — the upstream `await this.writeQueue` is unnecessary
    /// because this port writes inline (see module header).
    pub fn reload(&mut self) {
        let (global_settings, global_error) =
            Self::try_load_from_storage(&mut *self.storage, SettingsScope::Global, true);
        match global_error {
            None => {
                self.global_settings = global_settings;
                self.global_settings_load_error = None;
            }
            Some(error) => {
                self.errors.push(SettingsError {
                    scope: SettingsScope::Global,
                    error: PirError::Settings(error.to_string()),
                });
                self.global_settings_load_error = Some(error);
            }
        }

        self.modified_fields.clear();
        self.modified_nested_fields.clear();
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();

        let (project_settings, project_error) = Self::try_load_from_storage(
            &mut *self.storage,
            SettingsScope::Project,
            self.project_trusted,
        );
        match project_error {
            None => {
                self.project_settings = project_settings;
                self.project_settings_load_error = None;
            }
            Some(error) => {
                self.errors.push(SettingsError {
                    scope: SettingsScope::Project,
                    error: PirError::Settings(error.to_string()),
                });
                self.project_settings_load_error = Some(error);
            }
        }

        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// `applyOverrides` (settings-manager.ts:507-510): apply additional
    /// overrides on top of current settings (in-memory only).
    pub fn apply_overrides(&mut self, overrides: &Settings) {
        self.settings = deep_merge_settings(&self.settings, overrides);
    }

    /// `markModified` (settings-manager.ts:513-521).
    fn mark_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    /// `markProjectModified` (settings-manager.ts:523-532).
    fn mark_project_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_project_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_project_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    /// `assertProjectTrustedForWrite` (settings-manager.ts:534-538).
    fn assert_project_trusted_for_write(&self) -> Result<(), PirError> {
        if !self.project_trusted {
            return Err(PirError::Settings(
                "Project is not trusted; refusing to write project settings".to_string(),
            ));
        }
        Ok(())
    }

    /// `clearModifiedScope` (settings-manager.ts:545-554).
    fn clear_modified_scope(&mut self, scope: SettingsScope) {
        match scope {
            SettingsScope::Global => {
                self.modified_fields.clear();
                self.modified_nested_fields.clear();
            }
            SettingsScope::Project => {
                self.modified_project_fields.clear();
                self.modified_project_nested_fields.clear();
            }
        }
    }

    /// `enqueueWrite` (settings-manager.ts:556-568) — synchronous port:
    /// the task runs inline; the trust check happens at execution time
    /// (matching upstream's queue-time re-check); modified tracking clears
    /// only on success; failures are recorded per scope.
    fn enqueue_write(
        &mut self,
        scope: SettingsScope,
        snapshot_settings: Settings,
        modified_fields: HashSet<String>,
        modified_nested_fields: HashMap<String, HashSet<String>>,
    ) {
        if scope == SettingsScope::Project {
            if let Err(error) = self.assert_project_trusted_for_write() {
                self.errors.push(SettingsError { scope, error });
                return;
            }
        }
        match self.persist_scoped_settings(
            scope,
            &snapshot_settings,
            &modified_fields,
            &modified_nested_fields,
        ) {
            Ok(()) => self.clear_modified_scope(scope),
            Err(error) => self.errors.push(SettingsError { scope, error }),
        }
    }

    /// `persistScopedSettings` (settings-manager.ts:578-607): starts from
    /// the current disk content (re-migrated), overwrites only the fields
    /// modified this session — nested objects per modified sub-key — and
    /// serializes with `JSON.stringify(obj, null, 2)` (2-space indent, no
    /// trailing newline, insertion key order).
    fn persist_scoped_settings(
        &mut self,
        scope: SettingsScope,
        snapshot_settings: &Settings,
        modified_fields: &HashSet<String>,
        modified_nested_fields: &HashMap<String, HashSet<String>>,
    ) -> Result<(), PirError> {
        self.storage.with_lock(scope, &mut |current| {
            let mut merged_settings = match current {
                Some(content) if !content.is_empty() => {
                    let value: Value = serde_json::from_str(content)?;
                    let mut map = match value {
                        Value::Object(map) => map,
                        _ => {
                            return Err(PirError::Settings(
                                "Failed to parse settings: top level is not a JSON object"
                                    .to_string(),
                            ));
                        }
                    };
                    migrate_settings(&mut map);
                    map
                }
                _ => Map::new(),
            };

            for field in modified_fields {
                let value = snapshot_settings.get(field);
                match value {
                    // `mergedSettings[field] = undefined` upstream — the key
                    // is dropped from the JSON output (e.g. setShellPath(undefined)).
                    None => {
                        merged_settings.shift_remove(field);
                    }
                    Some(value) => {
                        if modified_nested_fields.contains_key(field) && value.is_object() {
                            let nested_modified = &modified_nested_fields[field];
                            let mut merged_nested = match merged_settings.get(field) {
                                Some(Value::Object(map)) => map.clone(),
                                _ => Map::new(),
                            };
                            if let Value::Object(in_memory_nested) = value {
                                for nested_key in nested_modified {
                                    match in_memory_nested.get(nested_key) {
                                        Some(nested_value) => {
                                            merged_nested
                                                .insert(nested_key.clone(), nested_value.clone());
                                        }
                                        None => {
                                            merged_nested.shift_remove(nested_key);
                                        }
                                    }
                                }
                            }
                            merged_settings.insert(field.clone(), Value::Object(merged_nested));
                        } else {
                            merged_settings.insert(field.clone(), value.clone());
                        }
                    }
                }
            }

            serde_json::to_string_pretty(&Value::Object(merged_settings))
                .map(Some)
                .map_err(PirError::Json)
        })
    }

    /// `save` (settings-manager.ts:609-623): persists the global scope; a
    /// prior global load error blocks the write so a broken file is never
    /// overwritten with partial data.
    fn save(&mut self) {
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);

        if self.global_settings_load_error.is_some() {
            return;
        }

        let snapshot = self.global_settings.clone();
        let modified_fields = self.modified_fields.clone();
        let modified_nested_fields = self.modified_nested_fields.clone();
        self.enqueue_write(
            SettingsScope::Global,
            snapshot,
            modified_fields,
            modified_nested_fields,
        );
    }

    /// `saveProjectSettings` (settings-manager.ts:625-640).
    fn save_project_settings(&mut self, settings: Settings) -> Result<(), PirError> {
        self.assert_project_trusted_for_write()?;
        self.project_settings = settings;
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);

        if self.project_settings_load_error.is_some() {
            return Ok(());
        }

        let snapshot = self.project_settings.clone();
        let modified_fields = self.modified_project_fields.clone();
        let modified_nested_fields = self.modified_project_nested_fields.clone();
        self.enqueue_write(
            SettingsScope::Project,
            snapshot,
            modified_fields,
            modified_nested_fields,
        );
        Ok(())
    }

    /// `updateProjectSettings` (settings-manager.ts:642-648).
    fn update_project_settings(
        &mut self,
        field: &str,
        update: impl FnOnce(&mut Settings),
    ) -> Result<(), PirError> {
        self.assert_project_trusted_for_write()?;
        let mut project_settings = self.project_settings.clone();
        update(&mut project_settings);
        self.mark_project_modified(field, None);
        self.save_project_settings(project_settings)
    }

    /// `flush` (settings-manager.ts:650-652) — no-op: this port writes
    /// synchronously inside each setter (see module header).
    pub fn flush(&mut self) {}

    /// `drainErrors` (settings-manager.ts:654-658).
    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    // =======================================================================
    // Getters / setters (settings-manager.ts:660-1234)
    // =======================================================================

    /// `getLastChangelogVersion` (settings-manager.ts:660-662).
    pub fn get_last_changelog_version(&self) -> Option<String> {
        self.settings
            .get_str("lastChangelogVersion")
            .map(str::to_string)
    }

    /// `setLastChangelogVersion` (settings-manager.ts:664-668).
    pub fn set_last_changelog_version(&mut self, version: &str) {
        self.global_settings
            .set("lastChangelogVersion", Value::String(version.to_string()));
        self.mark_modified("lastChangelogVersion", None);
        self.save();
    }

    /// `getSessionDir` (settings-manager.ts:670-673): `normalizePath` (tilde
    /// expansion) applied to a non-empty value.
    pub fn get_session_dir(&self) -> Option<String> {
        self.normalized_optional("sessionDir")
    }

    /// `sessionDir ? normalizePath(sessionDir) : sessionDir`
    /// (settings-manager.ts:672, 880).
    fn normalized_optional(&self, key: &str) -> Option<String> {
        match self.settings.get_str(key) {
            Some(value) if !value.is_empty() => Some(normalize_path(value)),
            other => other.map(str::to_string),
        }
    }

    /// `getDefaultProvider` (settings-manager.ts:675-677).
    pub fn get_default_provider(&self) -> Option<String> {
        self.settings.get_str("defaultProvider").map(str::to_string)
    }

    /// `getDefaultModel` (settings-manager.ts:679-681).
    pub fn get_default_model(&self) -> Option<String> {
        self.settings.get_str("defaultModel").map(str::to_string)
    }

    /// `setDefaultProvider` (settings-manager.ts:683-687).
    pub fn set_default_provider(&mut self, provider: &str) {
        self.global_settings
            .set("defaultProvider", Value::String(provider.to_string()));
        self.mark_modified("defaultProvider", None);
        self.save();
    }

    /// `setDefaultModel` (settings-manager.ts:689-693).
    pub fn set_default_model(&mut self, model_id: &str) {
        self.global_settings
            .set("defaultModel", Value::String(model_id.to_string()));
        self.mark_modified("defaultModel", None);
        self.save();
    }

    /// `setDefaultModelAndProvider` (settings-manager.ts:695-701).
    pub fn set_default_model_and_provider(&mut self, provider: &str, model_id: &str) {
        self.global_settings
            .set("defaultProvider", Value::String(provider.to_string()));
        self.global_settings
            .set("defaultModel", Value::String(model_id.to_string()));
        self.mark_modified("defaultProvider", None);
        self.mark_modified("defaultModel", None);
        self.save();
    }

    /// `getSteeringMode` (settings-manager.ts:703-705) —
    /// `this.settings.steeringMode || "one-at-a-time"`. The value type is
    /// [`QueueMode`]: the setting is the renamed `queueMode`
    /// (migration 1).
    pub fn get_steering_mode(&self) -> QueueMode {
        match self.settings.get_str("steeringMode") {
            Some("all") => QueueMode::All,
            _ => QueueMode::OneAtATime,
        }
    }

    /// `setSteeringMode` (settings-manager.ts:707-711).
    pub fn set_steering_mode(&mut self, mode: QueueMode) {
        self.global_settings.set("steeringMode", json_value(&mode));
        self.mark_modified("steeringMode", None);
        self.save();
    }

    /// `getFollowUpMode` (settings-manager.ts:713-715).
    pub fn get_follow_up_mode(&self) -> QueueMode {
        match self.settings.get_str("followUpMode") {
            Some("all") => QueueMode::All,
            _ => QueueMode::OneAtATime,
        }
    }

    /// `setFollowUpMode` (settings-manager.ts:717-721).
    pub fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.global_settings.set("followUpMode", json_value(&mode));
        self.mark_modified("followUpMode", None);
        self.save();
    }

    /// `getThemeSetting` (settings-manager.ts:723-727): the raw theme value,
    /// including slash-separated auto themes.
    pub fn get_theme_setting(&self) -> Option<String> {
        self.settings.get_str("theme").map(str::to_string)
    }

    /// `getTheme` (settings-manager.ts:729-732): slash-separated automatic
    /// theme settings (e.g. `"light/dark"`) are not fixed theme names and
    /// return `None` here.
    pub fn get_theme(&self) -> Option<String> {
        let theme = self.get_theme_setting()?;
        if theme.contains('/') {
            None
        } else {
            Some(theme)
        }
    }

    /// `setTheme` (settings-manager.ts:734-738).
    pub fn set_theme(&mut self, theme: &str) {
        self.global_settings
            .set("theme", Value::String(theme.to_string()));
        self.mark_modified("theme", None);
        self.save();
    }

    /// `getDefaultThinkingLevel` (settings-manager.ts:740-742).
    pub fn get_default_thinking_level(&self) -> Option<ThinkingLevel> {
        self.settings
            .get("defaultThinkingLevel")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// `setDefaultThinkingLevel` (settings-manager.ts:744-748).
    pub fn set_default_thinking_level(&mut self, level: ThinkingLevel) {
        self.global_settings
            .set("defaultThinkingLevel", json_value(&level));
        self.mark_modified("defaultThinkingLevel", None);
        self.save();
    }

    /// `getTransport` (settings-manager.ts:750-752) — `?? "auto"`.
    pub fn get_transport(&self) -> TransportSetting {
        self.settings
            .get("transport")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(Transport::Auto)
    }

    /// `setTransport` (settings-manager.ts:754-758).
    pub fn set_transport(&mut self, transport: TransportSetting) {
        self.global_settings
            .set("transport", json_value(&transport));
        self.mark_modified("transport", None);
        self.save();
    }

    /// `getCompactionEnabled` (settings-manager.ts:760-762) — default true.
    pub fn get_compaction_enabled(&self) -> bool {
        self.settings
            .nested_bool("compaction", "enabled")
            .unwrap_or(true)
    }

    /// `setCompactionEnabled` (settings-manager.ts:764-771).
    pub fn set_compaction_enabled(&mut self, enabled: bool) {
        self.global_settings
            .set_nested("compaction", "enabled", Value::Bool(enabled));
        self.mark_modified("compaction", Some("enabled"));
        self.save();
    }

    /// `getCompactionReserveTokens` (settings-manager.ts:773-775) —
    /// default 16384.
    pub fn get_compaction_reserve_tokens(&self) -> u64 {
        value_u64(self.settings.nested("compaction", "reserveTokens")).unwrap_or(16384)
    }

    /// `getCompactionKeepRecentTokens` (settings-manager.ts:777-779) —
    /// default 20000.
    pub fn get_compaction_keep_recent_tokens(&self) -> u64 {
        value_u64(self.settings.nested("compaction", "keepRecentTokens")).unwrap_or(20000)
    }

    /// `getCompactionSettings` (settings-manager.ts:781-787) — resolved
    /// values with defaults applied.
    pub fn get_compaction_settings(&self) -> CompactionConfig {
        CompactionConfig {
            enabled: self.get_compaction_enabled(),
            reserve_tokens: self.get_compaction_reserve_tokens(),
            keep_recent_tokens: self.get_compaction_keep_recent_tokens(),
        }
    }

    /// `getBranchSummarySettings` (settings-manager.ts:789-794) — resolved
    /// values with defaults applied.
    pub fn get_branch_summary_settings(&self) -> BranchSummaryConfig {
        BranchSummaryConfig {
            reserve_tokens: value_u64(self.settings.nested("branchSummary", "reserveTokens"))
                .unwrap_or(16384),
            skip_prompt: self.get_branch_summary_skip_prompt(),
        }
    }

    /// `getBranchSummarySkipPrompt` (settings-manager.ts:796-798) —
    /// default false.
    pub fn get_branch_summary_skip_prompt(&self) -> bool {
        self.settings
            .nested_bool("branchSummary", "skipPrompt")
            .unwrap_or(false)
    }

    /// `getRetryEnabled` (settings-manager.ts:800-802) — default true.
    pub fn get_retry_enabled(&self) -> bool {
        self.settings
            .nested_bool("retry", "enabled")
            .unwrap_or(true)
    }

    /// `setRetryEnabled` (settings-manager.ts:804-811).
    pub fn set_retry_enabled(&mut self, enabled: bool) {
        self.global_settings
            .set_nested("retry", "enabled", Value::Bool(enabled));
        self.mark_modified("retry", Some("enabled"));
        self.save();
    }

    /// `getRetrySettings` (settings-manager.ts:813-819) — resolved values
    /// with defaults applied.
    pub fn get_retry_settings(&self) -> RetryConfig {
        RetryConfig {
            enabled: self.get_retry_enabled(),
            max_retries: value_u64(self.settings.nested("retry", "maxRetries")).unwrap_or(3),
            base_delay_ms: value_u64(self.settings.nested("retry", "baseDelayMs")).unwrap_or(2000),
        }
    }

    /// `getHttpIdleTimeoutMs` (settings-manager.ts:821-823) — default
    /// [`DEFAULT_HTTP_IDLE_TIMEOUT_MS`]; a present-but-invalid value is an
    /// error (upstream throws).
    pub fn get_http_idle_timeout_ms(&self) -> Result<u64, PirError> {
        Ok(
            parse_timeout_setting(self.settings.get("httpIdleTimeoutMs"), "httpIdleTimeoutMs")?
                .unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
        )
    }

    /// `setHttpIdleTimeoutMs` (settings-manager.ts:825-832). Upstream
    /// validates `Number.isFinite && >= 0` and floors; the `u64` type makes
    /// that unrepresentable here.
    pub fn set_http_idle_timeout_ms(&mut self, timeout_ms: u64) {
        self.global_settings
            .set("httpIdleTimeoutMs", Value::Number(timeout_ms.into()));
        self.mark_modified("httpIdleTimeoutMs", None);
        self.save();
    }

    /// `getProviderRetrySettings` (settings-manager.ts:834-840). Note:
    /// upstream returns `maxRetries` as-is (`undefined` when unset — the SDK
    /// layer interprets it as 0, docs/settings.md:142) and defaults only
    /// `maxRetryDelayMs` to 60000.
    pub fn get_provider_retry_settings(&self) -> ProviderRetryConfig {
        let provider = self.settings.nested("retry", "provider");
        ProviderRetryConfig {
            timeout_ms: value_u64(provider.and_then(|p| p.get("timeoutMs"))),
            max_retries: value_u64(provider.and_then(|p| p.get("maxRetries"))),
            max_retry_delay_ms: value_u64(provider.and_then(|p| p.get("maxRetryDelayMs")))
                .unwrap_or(60000),
        }
    }

    /// `getWebSocketConnectTimeoutMs` (settings-manager.ts:842-844) — no
    /// default at this layer (docs/settings.md:172 documents 15000, applied
    /// by the SDK transport layer); a present-but-invalid value is an error.
    pub fn get_websocket_connect_timeout_ms(&self) -> Result<Option<u64>, PirError> {
        parse_timeout_setting(
            self.settings.get("websocketConnectTimeoutMs"),
            "websocketConnectTimeoutMs",
        )
    }

    /// `getHideThinkingBlock` (settings-manager.ts:846-848) — default false.
    pub fn get_hide_thinking_block(&self) -> bool {
        self.settings.get_bool("hideThinkingBlock").unwrap_or(false)
    }

    /// `getShowCacheMissNotices` (settings-manager.ts:850-852) —
    /// default false.
    pub fn get_show_cache_miss_notices(&self) -> bool {
        self.settings
            .get_bool("showCacheMissNotices")
            .unwrap_or(false)
    }

    /// `getExternalEditorCommand` (settings-manager.ts:854-864): non-blank
    /// `externalEditor` setting → `VISUAL` → `EDITOR` → platform default
    /// (`notepad` on Windows, `nano` elsewhere).
    pub fn get_external_editor_command(&self) -> String {
        if let Some(editor) = self.settings.get_str("externalEditor") {
            if !editor.trim().is_empty() {
                return editor.to_string();
            }
        }
        if let Some(editor) = environment::external_editor_from_env() {
            return editor;
        }
        if cfg!(windows) {
            "notepad".to_string()
        } else {
            "nano".to_string()
        }
    }

    /// `setHideThinkingBlock` (settings-manager.ts:866-870).
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.global_settings
            .set("hideThinkingBlock", Value::Bool(hide));
        self.mark_modified("hideThinkingBlock", None);
        self.save();
    }

    /// `setShowCacheMissNotices` (settings-manager.ts:872-876).
    pub fn set_show_cache_miss_notices(&mut self, show: bool) {
        self.global_settings
            .set("showCacheMissNotices", Value::Bool(show));
        self.mark_modified("showCacheMissNotices", None);
        self.save();
    }

    /// `getShellPath` (settings-manager.ts:878-881): `normalizePath` applied
    /// to a non-empty value.
    pub fn get_shell_path(&self) -> Option<String> {
        self.normalized_optional("shellPath")
    }

    /// `setShellPath` (settings-manager.ts:883-887). `None` assigns
    /// `undefined` upstream, which drops the key from the persisted JSON.
    pub fn set_shell_path(&mut self, path: Option<String>) {
        match path {
            Some(path) => self.global_settings.set("shellPath", Value::String(path)),
            None => self.global_settings.remove("shellPath"),
        }
        self.mark_modified("shellPath", None);
        self.save();
    }

    /// `getQuietStartup` (settings-manager.ts:889-891) — default false.
    pub fn get_quiet_startup(&self) -> bool {
        self.settings.get_bool("quietStartup").unwrap_or(false)
    }

    /// `setQuietStartup` (settings-manager.ts:893-897).
    pub fn set_quiet_startup(&mut self, quiet: bool) {
        self.global_settings.set("quietStartup", Value::Bool(quiet));
        self.mark_modified("quietStartup", None);
        self.save();
    }

    /// `getDefaultProjectTrust` (settings-manager.ts:899-902): reads the
    /// **global** settings only; anything but `"always"`/`"never"` is `"ask"`.
    pub fn get_default_project_trust(&self) -> DefaultProjectTrust {
        match self.global_settings.get_str("defaultProjectTrust") {
            Some("always") => DefaultProjectTrust::Always,
            Some("never") => DefaultProjectTrust::Never,
            _ => DefaultProjectTrust::Ask,
        }
    }

    /// `setDefaultProjectTrust` (settings-manager.ts:904-908).
    pub fn set_default_project_trust(&mut self, default_project_trust: DefaultProjectTrust) {
        self.global_settings
            .set("defaultProjectTrust", json_value(&default_project_trust));
        self.mark_modified("defaultProjectTrust", None);
        self.save();
    }

    /// `getShellCommandPrefix` (settings-manager.ts:910-912).
    pub fn get_shell_command_prefix(&self) -> Option<String> {
        self.settings
            .get_str("shellCommandPrefix")
            .map(str::to_string)
    }

    /// `setShellCommandPrefix` (settings-manager.ts:914-918). `None` drops
    /// the key (see [`SettingsManager::set_shell_path`]).
    pub fn set_shell_command_prefix(&mut self, prefix: Option<String>) {
        match prefix {
            Some(prefix) => self
                .global_settings
                .set("shellCommandPrefix", Value::String(prefix)),
            None => self.global_settings.remove("shellCommandPrefix"),
        }
        self.mark_modified("shellCommandPrefix", None);
        self.save();
    }

    /// `getNpmCommand` (settings-manager.ts:920-928) — returns a copy.
    pub fn get_npm_command(&self) -> Option<Vec<String>> {
        let array = self.settings.get("npmCommand")?.as_array()?;
        Some(
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    }

    /// `setNpmCommand` (settings-manager.ts:924-928). `None` drops the key.
    pub fn set_npm_command(&mut self, command: Option<Vec<String>>) {
        match command {
            Some(command) => self.global_settings.set(
                "npmCommand",
                Value::Array(command.into_iter().map(Value::String).collect()),
            ),
            None => self.global_settings.remove("npmCommand"),
        }
        self.mark_modified("npmCommand", None);
        self.save();
    }

    /// `getCollapseChangelog` (settings-manager.ts:930-932) — default false.
    pub fn get_collapse_changelog(&self) -> bool {
        self.settings.get_bool("collapseChangelog").unwrap_or(false)
    }

    /// `setCollapseChangelog` (settings-manager.ts:934-938).
    pub fn set_collapse_changelog(&mut self, collapse: bool) {
        self.global_settings
            .set("collapseChangelog", Value::Bool(collapse));
        self.mark_modified("collapseChangelog", None);
        self.save();
    }

    /// `getEnableInstallTelemetry` (settings-manager.ts:940-942) —
    /// default true.
    pub fn get_enable_install_telemetry(&self) -> bool {
        self.settings
            .get_bool("enableInstallTelemetry")
            .unwrap_or(true)
    }

    /// `setEnableInstallTelemetry` (settings-manager.ts:944-948).
    pub fn set_enable_install_telemetry(&mut self, enabled: bool) {
        self.global_settings
            .set("enableInstallTelemetry", Value::Bool(enabled));
        self.mark_modified("enableInstallTelemetry", None);
        self.save();
    }

    /// `getEnableAnalytics` (settings-manager.ts:950-952) — default false.
    pub fn get_enable_analytics(&self) -> bool {
        self.settings.get_bool("enableAnalytics").unwrap_or(false)
    }

    /// `getTrackingId` (settings-manager.ts:954-956).
    pub fn get_tracking_id(&self) -> Option<String> {
        self.settings.get_str("trackingId").map(str::to_string)
    }

    /// `setEnableAnalytics` (settings-manager.ts:958-967): generates a
    /// tracking identifier (UUID v4) on first opt-in — when the stored
    /// `trackingId` is absent, null, or empty (JS falsy check).
    pub fn set_enable_analytics(&mut self, enabled: bool) {
        self.global_settings
            .set("enableAnalytics", Value::Bool(enabled));
        self.mark_modified("enableAnalytics", None);
        let has_tracking_id = self
            .global_settings
            .get_str("trackingId")
            .map(|id| !id.is_empty())
            .unwrap_or(false);
        if enabled && !has_tracking_id {
            self.global_settings
                .set("trackingId", Value::String(random_uuid_v4()));
            self.mark_modified("trackingId", None);
        }
        self.save();
    }

    /// `getPackages` (settings-manager.ts:969-971) — returns a copy.
    /// Elements that match neither the string nor the object form are
    /// skipped (upstream passes them through raw).
    pub fn get_packages(&self) -> Vec<PackageSource> {
        self.settings
            .get("packages")
            .and_then(Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `setPackages` (settings-manager.ts:973-977).
    pub fn set_packages(&mut self, packages: Vec<PackageSource>) {
        let array = packages.iter().map(json_value).collect();
        self.global_settings.set("packages", Value::Array(array));
        self.mark_modified("packages", None);
        self.save();
    }

    /// `setProjectPackages` (settings-manager.ts:979-983).
    pub fn set_project_packages(&mut self, packages: Vec<PackageSource>) -> Result<(), PirError> {
        let array = packages.iter().map(json_value).collect();
        self.update_project_settings("packages", |settings| {
            settings.set("packages", Value::Array(array));
        })
    }

    /// `getExtensionPaths` (settings-manager.ts:985-987).
    pub fn get_extension_paths(&self) -> Vec<String> {
        self.string_array("extensions")
    }

    /// `setExtensionPaths` (settings-manager.ts:989-993).
    pub fn set_extension_paths(&mut self, paths: Vec<String>) {
        self.global_settings.set(
            "extensions",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
        self.mark_modified("extensions", None);
        self.save();
    }

    /// `setProjectExtensionPaths` (settings-manager.ts:995-999).
    pub fn set_project_extension_paths(&mut self, paths: Vec<String>) -> Result<(), PirError> {
        self.update_project_settings("extensions", |settings| {
            settings.set(
                "extensions",
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `getSkillPaths` (settings-manager.ts:1001-1003).
    pub fn get_skill_paths(&self) -> Vec<String> {
        self.string_array("skills")
    }

    /// `setSkillPaths` (settings-manager.ts:1005-1009).
    pub fn set_skill_paths(&mut self, paths: Vec<String>) {
        self.global_settings.set(
            "skills",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
        self.mark_modified("skills", None);
        self.save();
    }

    /// `setProjectSkillPaths` (settings-manager.ts:1011-1015).
    pub fn set_project_skill_paths(&mut self, paths: Vec<String>) -> Result<(), PirError> {
        self.update_project_settings("skills", |settings| {
            settings.set(
                "skills",
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `getPromptTemplatePaths` (settings-manager.ts:1017-1019).
    pub fn get_prompt_template_paths(&self) -> Vec<String> {
        self.string_array("prompts")
    }

    /// `setPromptTemplatePaths` (settings-manager.ts:1021-1025).
    pub fn set_prompt_template_paths(&mut self, paths: Vec<String>) {
        self.global_settings.set(
            "prompts",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
        self.mark_modified("prompts", None);
        self.save();
    }

    /// `setProjectPromptTemplatePaths` (settings-manager.ts:1027-1031).
    pub fn set_project_prompt_template_paths(
        &mut self,
        paths: Vec<String>,
    ) -> Result<(), PirError> {
        self.update_project_settings("prompts", |settings| {
            settings.set(
                "prompts",
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `getThemePaths` (settings-manager.ts:1033-1035).
    pub fn get_theme_paths(&self) -> Vec<String> {
        self.string_array("themes")
    }

    /// `setThemePaths` (settings-manager.ts:1037-1041).
    pub fn set_theme_paths(&mut self, paths: Vec<String>) {
        self.global_settings.set(
            "themes",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
        self.mark_modified("themes", None);
        self.save();
    }

    /// `setProjectThemePaths` (settings-manager.ts:1043-1047).
    pub fn set_project_theme_paths(&mut self, paths: Vec<String>) -> Result<(), PirError> {
        self.update_project_settings("themes", |settings| {
            settings.set(
                "themes",
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `[...(settings[key] ?? [])]` (settings-manager.ts:970,986,1002,1018,1034).
    /// A non-array value reads as empty instead of throwing (upstream throws
    /// a TypeError on such malformed data).
    fn string_array(&self, key: &str) -> Vec<String> {
        self.settings
            .get(key)
            .and_then(Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `getEnableSkillCommands` (settings-manager.ts:1049-1051) —
    /// default true.
    pub fn get_enable_skill_commands(&self) -> bool {
        self.settings
            .get_bool("enableSkillCommands")
            .unwrap_or(true)
    }

    /// `setEnableSkillCommands` (settings-manager.ts:1053-1057).
    pub fn set_enable_skill_commands(&mut self, enabled: bool) {
        self.global_settings
            .set("enableSkillCommands", Value::Bool(enabled));
        self.mark_modified("enableSkillCommands", None);
        self.save();
    }

    /// `getThinkingBudgets` (settings-manager.ts:1059-1061).
    pub fn get_thinking_budgets(&self) -> Option<ThinkingBudgetsSettings> {
        self.settings
            .get("thinkingBudgets")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// `getShowImages` (settings-manager.ts:1063-1065) — default true.
    pub fn get_show_images(&self) -> bool {
        self.settings
            .nested_bool("terminal", "showImages")
            .unwrap_or(true)
    }

    /// `setShowImages` (settings-manager.ts:1067-1074).
    pub fn set_show_images(&mut self, show: bool) {
        self.global_settings
            .set_nested("terminal", "showImages", Value::Bool(show));
        self.mark_modified("terminal", Some("showImages"));
        self.save();
    }

    /// `getImageWidthCells` (settings-manager.ts:1076-1082) — default 60;
    /// non-numeric values fall back to 60; `Math.max(1, Math.floor(w))`.
    pub fn get_image_width_cells(&self) -> u64 {
        match value_u64(self.settings.nested("terminal", "imageWidthCells")) {
            Some(width) => width.max(1),
            None => 60,
        }
    }

    /// `setImageWidthCells` (settings-manager.ts:1084-1091) —
    /// `Math.max(1, Math.floor(width))` (floor is implicit in `u64`).
    pub fn set_image_width_cells(&mut self, width: u64) {
        self.global_settings.set_nested(
            "terminal",
            "imageWidthCells",
            Value::Number(width.max(1).into()),
        );
        self.mark_modified("terminal", Some("imageWidthCells"));
        self.save();
    }

    /// `getClearOnShrink` (settings-manager.ts:1093-1099): setting first,
    /// then the `PIR_CLEAR_ON_SHRINK === "1"` env fallback, default false.
    pub fn get_clear_on_shrink(&self) -> bool {
        if let Some(clear_on_shrink) = self.settings.nested_bool("terminal", "clearOnShrink") {
            return clear_on_shrink;
        }
        environment::clear_on_shrink_enabled()
    }

    /// `setClearOnShrink` (settings-manager.ts:1101-1108).
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.global_settings
            .set_nested("terminal", "clearOnShrink", Value::Bool(enabled));
        self.mark_modified("terminal", Some("clearOnShrink"));
        self.save();
    }

    /// `getShowTerminalProgress` (settings-manager.ts:1110-1112) —
    /// default false.
    pub fn get_show_terminal_progress(&self) -> bool {
        self.settings
            .nested_bool("terminal", "showTerminalProgress")
            .unwrap_or(false)
    }

    /// `setShowTerminalProgress` (settings-manager.ts:1114-1121).
    pub fn set_show_terminal_progress(&mut self, enabled: bool) {
        self.global_settings
            .set_nested("terminal", "showTerminalProgress", Value::Bool(enabled));
        self.mark_modified("terminal", Some("showTerminalProgress"));
        self.save();
    }

    /// `getImageAutoResize` (settings-manager.ts:1123-1125) — default true.
    pub fn get_image_auto_resize(&self) -> bool {
        self.settings
            .nested_bool("images", "autoResize")
            .unwrap_or(true)
    }

    /// `setImageAutoResize` (settings-manager.ts:1127-1134).
    pub fn set_image_auto_resize(&mut self, enabled: bool) {
        self.global_settings
            .set_nested("images", "autoResize", Value::Bool(enabled));
        self.mark_modified("images", Some("autoResize"));
        self.save();
    }

    /// `getBlockImages` (settings-manager.ts:1136-1138) — default false.
    pub fn get_block_images(&self) -> bool {
        self.settings
            .nested_bool("images", "blockImages")
            .unwrap_or(false)
    }

    /// `setBlockImages` (settings-manager.ts:1140-1147).
    pub fn set_block_images(&mut self, blocked: bool) {
        self.global_settings
            .set_nested("images", "blockImages", Value::Bool(blocked));
        self.mark_modified("images", Some("blockImages"));
        self.save();
    }

    /// `getEnabledModels` (settings-manager.ts:1149-1151).
    pub fn get_enabled_models(&self) -> Option<Vec<String>> {
        let array = self.settings.get("enabledModels")?.as_array()?;
        Some(
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    }

    /// `setEnabledModels` (settings-manager.ts:1153-1157). `None` drops the
    /// key.
    pub fn set_enabled_models(&mut self, patterns: Option<Vec<String>>) {
        match patterns {
            Some(patterns) => self.global_settings.set(
                "enabledModels",
                Value::Array(patterns.into_iter().map(Value::String).collect()),
            ),
            None => self.global_settings.remove("enabledModels"),
        }
        self.mark_modified("enabledModels", None);
        self.save();
    }

    /// `getDoubleEscapeAction` (settings-manager.ts:1159-1161) —
    /// default "tree".
    pub fn get_double_escape_action(&self) -> DoubleEscapeAction {
        match self.settings.get_str("doubleEscapeAction") {
            Some("fork") => DoubleEscapeAction::Fork,
            Some("none") => DoubleEscapeAction::None,
            _ => DoubleEscapeAction::Tree,
        }
    }

    /// `setDoubleEscapeAction` (settings-manager.ts:1163-1167).
    pub fn set_double_escape_action(&mut self, action: DoubleEscapeAction) {
        self.global_settings
            .set("doubleEscapeAction", json_value(&action));
        self.mark_modified("doubleEscapeAction", None);
        self.save();
    }

    /// `getTreeFilterMode` (settings-manager.ts:1169-1173): values outside
    /// the valid list fall back to "default".
    pub fn get_tree_filter_mode(&self) -> TreeFilterMode {
        match self.settings.get_str("treeFilterMode") {
            Some("no-tools") => TreeFilterMode::NoTools,
            Some("user-only") => TreeFilterMode::UserOnly,
            Some("labeled-only") => TreeFilterMode::LabeledOnly,
            Some("all") => TreeFilterMode::All,
            _ => TreeFilterMode::Default,
        }
    }

    /// `setTreeFilterMode` (settings-manager.ts:1175-1179).
    pub fn set_tree_filter_mode(&mut self, mode: TreeFilterMode) {
        self.global_settings
            .set("treeFilterMode", json_value(&mode));
        self.mark_modified("treeFilterMode", None);
        self.save();
    }

    /// `getShowHardwareCursor` (settings-manager.ts:1181-1183): setting,
    /// then the `PIR_HARDWARE_CURSOR === "1"` env fallback.
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.settings
            .get_bool("showHardwareCursor")
            .unwrap_or_else(environment::hardware_cursor_enabled)
    }

    /// `setShowHardwareCursor` (settings-manager.ts:1185-1189).
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.global_settings
            .set("showHardwareCursor", Value::Bool(enabled));
        self.mark_modified("showHardwareCursor", None);
        self.save();
    }

    /// `getEditorPaddingX` (settings-manager.ts:1191-1193) — default 0.
    pub fn get_editor_padding_x(&self) -> u64 {
        value_u64(self.settings.get("editorPaddingX")).unwrap_or(0)
    }

    /// `setEditorPaddingX` (settings-manager.ts:1195-1199) —
    /// `Math.max(0, Math.min(3, Math.floor(v)))`.
    pub fn set_editor_padding_x(&mut self, padding: u64) {
        self.global_settings
            .set("editorPaddingX", Value::Number(padding.min(3).into()));
        self.mark_modified("editorPaddingX", None);
        self.save();
    }

    /// `getOutputPad` (settings-manager.ts:1201-1203) — `=== 0 ? 0 : 1`:
    /// any non-zero (or missing) value yields 1.
    pub fn get_output_pad(&self) -> u8 {
        if self.settings.get("outputPad").and_then(Value::as_f64) == Some(0.0) {
            0
        } else {
            1
        }
    }

    /// `setOutputPad` (settings-manager.ts:1205-1209).
    pub fn set_output_pad(&mut self, padding: u8) {
        self.global_settings
            .set("outputPad", Value::Number(u64::from(padding).into()));
        self.mark_modified("outputPad", None);
        self.save();
    }

    /// `getAutocompleteMaxVisible` (settings-manager.ts:1211-1213) —
    /// default 5.
    pub fn get_autocomplete_max_visible(&self) -> u64 {
        value_u64(self.settings.get("autocompleteMaxVisible")).unwrap_or(5)
    }

    /// `setAutocompleteMaxVisible` (settings-manager.ts:1215-1219) —
    /// `Math.max(3, Math.min(20, Math.floor(v)))`.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: u64) {
        self.global_settings.set(
            "autocompleteMaxVisible",
            Value::Number(max_visible.clamp(3, 20).into()),
        );
        self.mark_modified("autocompleteMaxVisible", None);
        self.save();
    }

    /// `getCodeBlockIndent` (settings-manager.ts:1221-1223) —
    /// default two spaces.
    pub fn get_code_block_indent(&self) -> String {
        self.settings
            .nested("markdown", "codeBlockIndent")
            .and_then(Value::as_str)
            .unwrap_or("  ")
            .to_string()
    }

    /// `getWarnings` (settings-manager.ts:1225-1227) — returns a copy;
    /// defaults are applied by consumers (`anthropicExtraUsage`: true).
    pub fn get_warnings(&self) -> WarningSettings {
        self.settings
            .get("warnings")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// `setWarnings` (settings-manager.ts:1229-1233).
    pub fn set_warnings(&mut self, warnings: &WarningSettings) {
        self.global_settings.set("warnings", json_value(warnings));
        self.mark_modified("warnings", None);
        self.save();
    }
}

/// Resolved return type of [`SettingsManager::get_compaction_settings`]
/// (settings-manager.ts:781-787).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

/// Resolved return type of [`SettingsManager::get_branch_summary_settings`]
/// (settings-manager.ts:789-794).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchSummaryConfig {
    pub reserve_tokens: u64,
    pub skip_prompt: bool,
}

/// Resolved return type of [`SettingsManager::get_retry_settings`]
/// (settings-manager.ts:813-819).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    pub enabled: bool,
    pub max_retries: u64,
    pub base_delay_ms: u64,
}

/// Resolved return type of [`SettingsManager::get_provider_retry_settings`]
/// (settings-manager.ts:834-840).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRetryConfig {
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u64>,
    pub max_retry_delay_ms: u64,
}

// ---------------------------------------------------------------------------
// randomUUID (settings-manager.ts:962)
// ---------------------------------------------------------------------------

/// `crypto.randomUUID()` — UUID v4. Entropy comes from `/dev/urandom` on
/// unix; the fallback mixes time, pid, and a counter (not cryptographically
/// secure — see module header).
fn random_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    fill_random_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn fill_random_bytes(bytes: &mut [u8; 16]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            if file.read_exact(bytes).is_ok() {
                return;
            }
        }
    }
    // Fallback: time/pid/counter mix (same approach as tools::random_hex_16).
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let lo = (nanos as u64) ^ ((std::process::id() as u64) << 32);
    let hi = ((nanos >> 64) as u64).wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed));
    bytes[..8].copy_from_slice(&lo.to_le_bytes());
    bytes[8..].copy_from_slice(&hi.to_le_bytes());
}

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/settings-manager.test.ts` (511
    //! lines) plus unit tests for `deepMergeSettings` and the four legacy
    //! migrations. `flush()` calls from the upstream async tests are omitted
    //! (this port writes synchronously). Env-manipulating tests are
    //! serialized through `ENV_LOCK` (the process environment is global).
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    use serde_json::json;

    use super::*;
    use crate::tools::test_helpers::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> (MutexGuard<'static, ()>, Self) {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = vars
                .iter()
                .map(|(name, _)| (*name, std::env::var(name).ok()))
                .collect();
            for (name, value) in vars {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
            (lock, EnvGuard { saved })
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    struct TestDirs {
        _tmp: TempDir,
        agent_dir: PathBuf,
        project_dir: PathBuf,
    }

    /// Upstream `beforeEach`: fresh agent dir and `project/.pir`.
    fn test_dirs() -> TestDirs {
        let tmp = TempDir::new();
        let agent_dir = tmp.path().join("agent");
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(project_dir.join(".pir")).unwrap();
        TestDirs {
            _tmp: tmp,
            agent_dir,
            project_dir,
        }
    }

    fn global_path(dirs: &TestDirs) -> PathBuf {
        dirs.agent_dir.join("settings.json")
    }

    fn project_path(dirs: &TestDirs) -> PathBuf {
        dirs.project_dir.join(".pir").join("settings.json")
    }

    fn write_json(path: &Path, value: Value) {
        std::fs::write(path, serde_json::to_string(&value).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn create(dirs: &TestDirs) -> SettingsManager {
        SettingsManager::create(
            &dirs.project_dir,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions::default(),
        )
    }

    fn settings(value: Value) -> Settings {
        Settings::from_map(value.as_object().unwrap().clone())
    }

    // =======================================================================
    // describe("preserves externally added settings")
    // =======================================================================

    // Port of "should preserve enabledModels when changing thinking level".
    #[test]
    fn test_preserves_enabled_models_when_changing_thinking_level() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"theme": "dark", "defaultModel": "claude-sonnet"}),
        );

        let mut manager = create(&dirs);

        // User edits settings.json externally to add enabledModels.
        let mut current = read_json(&global_path(&dirs));
        current["enabledModels"] = json!(["claude-opus-4-5", "gpt-5.2-codex"]);
        write_json(&global_path(&dirs), current);

        manager.set_default_thinking_level(ThinkingLevel::High);

        let saved = read_json(&global_path(&dirs));
        assert_eq!(
            saved["enabledModels"],
            json!(["claude-opus-4-5", "gpt-5.2-codex"])
        );
        assert_eq!(saved["defaultThinkingLevel"], json!("high"));
        assert_eq!(saved["theme"], json!("dark"));
        assert_eq!(saved["defaultModel"], json!("claude-sonnet"));
    }

    // Port of "should preserve custom settings when changing theme".
    #[test]
    fn test_preserves_custom_settings_when_changing_theme() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"defaultModel": "claude-sonnet"}),
        );

        let mut manager = create(&dirs);

        let mut current = read_json(&global_path(&dirs));
        current["shellPath"] = json!("/bin/zsh");
        current["extensions"] = json!(["/path/to/extension.ts"]);
        write_json(&global_path(&dirs), current);

        manager.set_theme("light");

        let saved = read_json(&global_path(&dirs));
        assert_eq!(saved["shellPath"], json!("/bin/zsh"));
        assert_eq!(saved["extensions"], json!(["/path/to/extension.ts"]));
        assert_eq!(saved["theme"], json!("light"));
    }

    // Port of "should let in-memory changes override file changes for same key".
    #[test]
    fn test_in_memory_changes_override_file_changes_for_same_key() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));

        let mut manager = create(&dirs);

        let mut current = read_json(&global_path(&dirs));
        current["defaultThinkingLevel"] = json!("low");
        write_json(&global_path(&dirs), current);

        manager.set_default_thinking_level(ThinkingLevel::High);

        let saved = read_json(&global_path(&dirs));
        assert_eq!(saved["defaultThinkingLevel"], json!("high"));
    }

    // =======================================================================
    // describe("packages migration")
    // =======================================================================

    // Port of "should keep local-only extensions in extensions array".
    #[test]
    fn test_keeps_local_only_extensions_in_extensions_array() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"extensions": ["/local/ext.ts", "./relative/ext.ts"]}),
        );

        let manager = create(&dirs);

        assert_eq!(manager.get_packages(), Vec::<PackageSource>::new());
        assert_eq!(
            manager.get_extension_paths(),
            vec!["/local/ext.ts".to_string(), "./relative/ext.ts".to_string()]
        );
    }

    // Port of "should handle packages with filtering objects".
    #[test]
    fn test_handles_packages_with_filtering_objects() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({
                "packages": [
                    "npm:simple-pkg",
                    {"source": "npm:shitty-extensions", "extensions": ["extensions/oracle.ts"], "skills": []}
                ]
            }),
        );

        let manager = create(&dirs);

        let packages = manager.get_packages();
        assert_eq!(packages.len(), 2);
        assert_eq!(
            packages[0],
            PackageSource::Source("npm:simple-pkg".to_string())
        );
        assert_eq!(
            packages[1],
            PackageSource::Filtered(PackageSourceFilter {
                source: "npm:shitty-extensions".to_string(),
                autoload: None,
                extensions: Some(vec!["extensions/oracle.ts".to_string()]),
                skills: Some(Vec::new()),
                prompts: None,
                themes: None,
            })
        );
    }

    // =======================================================================
    // describe("reload")
    // =======================================================================

    // Port of "should reload global settings from disk".
    #[test]
    fn test_reloads_global_settings_from_disk() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"theme": "dark", "extensions": ["/before.ts"]}),
        );

        let mut manager = create(&dirs);

        write_json(
            &global_path(&dirs),
            json!({"theme": "light", "extensions": ["/after.ts"], "defaultModel": "claude-sonnet"}),
        );

        manager.reload();

        assert_eq!(manager.get_theme().as_deref(), Some("light"));
        assert_eq!(manager.get_extension_paths(), vec!["/after.ts".to_string()]);
        assert_eq!(
            manager.get_default_model().as_deref(),
            Some("claude-sonnet")
        );
    }

    // Port of "should keep previous settings when file is invalid".
    #[test]
    fn test_keeps_previous_settings_when_file_invalid() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));

        let mut manager = create(&dirs);

        std::fs::write(global_path(&dirs), "{ invalid json").unwrap();
        manager.reload();

        assert_eq!(manager.get_theme().as_deref(), Some("dark"));
    }

    // =======================================================================
    // describe("theme setting")
    // =======================================================================

    // Port of "stores slash-separated automatic theme settings separately
    // from fixed theme names".
    #[test]
    fn test_slash_separated_theme_setting() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "light/dark"}));

        let mut manager = create(&dirs);

        assert_eq!(manager.get_theme(), None);
        assert_eq!(manager.get_theme_setting().as_deref(), Some("light/dark"));

        manager.set_theme("solarized-light/tokyo-night");

        let saved = read_json(&global_path(&dirs));
        assert_eq!(saved["theme"], json!("solarized-light/tokyo-night"));
    }

    // =======================================================================
    // describe("error tracking")
    // =======================================================================

    // Port of "should collect and clear load errors via drainErrors".
    #[test]
    fn test_drain_errors_collects_and_clears() {
        let dirs = test_dirs();
        std::fs::write(global_path(&dirs), "{ invalid global json").unwrap();
        std::fs::write(project_path(&dirs), "{ invalid project json").unwrap();

        let mut manager = create(&dirs);
        let errors = manager.drain_errors();

        assert_eq!(errors.len(), 2);
        let mut scopes: Vec<&str> = errors.iter().map(|e| e.scope.as_str()).collect();
        scopes.sort();
        assert_eq!(scopes, vec!["global", "project"]);
        assert!(manager.drain_errors().is_empty());
    }

    // =======================================================================
    // describe("project trust")
    // =======================================================================

    // Port of "should skip project settings when project is not trusted".
    #[test]
    fn test_skips_project_settings_when_not_trusted() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "global"}));
        write_json(&project_path(&dirs), json!({"theme": "project"}));

        let manager = SettingsManager::create(
            &dirs.project_dir,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: false,
            },
        );

        assert!(!manager.is_project_trusted());
        assert_eq!(manager.get_theme().as_deref(), Some("global"));
        assert_eq!(manager.get_project_settings(), Settings::new());
    }

    // Port of "should reload project settings after trust changes to true".
    #[test]
    fn test_reloads_project_settings_after_trust_true() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "global"}));
        write_json(&project_path(&dirs), json!({"theme": "project"}));
        let mut manager = SettingsManager::create(
            &dirs.project_dir,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: false,
            },
        );

        manager.set_project_trusted(true);

        assert!(manager.is_project_trusted());
        assert_eq!(manager.get_theme().as_deref(), Some("project"));
    }

    // Port of "should fail project settings writes when project is not trusted".
    #[test]
    fn test_fails_project_writes_when_not_trusted() {
        let dirs = test_dirs();
        write_json(&project_path(&dirs), json!({"packages": ["npm:existing"]}));
        let mut manager = SettingsManager::create(
            &dirs.project_dir,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: false,
            },
        );

        let result =
            manager.set_project_packages(vec![PackageSource::Source("npm:new".to_string())]);
        match result {
            Err(PirError::Settings(message)) => {
                assert_eq!(
                    message,
                    "Project is not trusted; refusing to write project settings"
                );
            }
            other => panic!("expected settings error, got {other:?}"),
        }

        assert_eq!(manager.get_project_settings(), Settings::new());
        assert_eq!(
            read_json(&project_path(&dirs)),
            json!({"packages": ["npm:existing"]})
        );
    }

    // Port of "should read default project trust from global settings only".
    #[test]
    fn test_default_project_trust_global_only() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"defaultProjectTrust": "always"}),
        );
        write_json(
            &project_path(&dirs),
            json!({"defaultProjectTrust": "never"}),
        );

        let manager = create(&dirs);

        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Always
        );
    }

    // Port of "should default invalid project trust settings to ask".
    #[test]
    fn test_invalid_project_trust_defaults_to_ask() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"defaultProjectTrust": "sometimes"}),
        );

        let manager = create(&dirs);

        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Ask
        );
    }

    // =======================================================================
    // describe("project settings directory creation")
    // =======================================================================

    // Port of "should not create .pi folder when only reading project settings".
    #[test]
    fn test_does_not_create_pir_dir_when_only_reading() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));

        std::fs::remove_dir_all(dirs.project_dir.join(".pir")).unwrap();

        let manager = create(&dirs);

        assert!(!dirs.project_dir.join(".pir").exists());
        assert_eq!(manager.get_theme().as_deref(), Some("dark"));
    }

    // Port of "should create .pi folder when writing project settings".
    #[test]
    fn test_creates_pir_dir_when_writing_project_settings() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));

        std::fs::remove_dir_all(dirs.project_dir.join(".pir")).unwrap();

        let mut manager = create(&dirs);

        assert!(!dirs.project_dir.join(".pir").exists());

        manager
            .set_project_packages(vec![PackageSource::Filtered(PackageSourceFilter {
                source: "npm:test-pkg".to_string(),
                ..PackageSourceFilter::default()
            })])
            .unwrap();

        assert!(dirs.project_dir.join(".pir").exists());
        assert!(project_path(&dirs).exists());
    }

    // =======================================================================
    // describe("httpIdleTimeoutMs")
    // =======================================================================

    // Port of "should default to 5 minutes".
    #[test]
    fn test_http_idle_timeout_defaults_to_five_minutes() {
        let dirs = test_dirs();
        let manager = create(&dirs);
        assert_eq!(
            manager.get_http_idle_timeout_ms().unwrap(),
            DEFAULT_HTTP_IDLE_TIMEOUT_MS
        );
    }

    // Port of "should use merged global and project settings".
    #[test]
    fn test_http_idle_timeout_merged_global_project() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"httpIdleTimeoutMs": 300000}));
        write_json(&project_path(&dirs), json!({"httpIdleTimeoutMs": 0}));

        let manager = create(&dirs);

        assert_eq!(manager.get_http_idle_timeout_ms().unwrap(), 0);
    }

    // Port of "should reject invalid timeout values".
    #[test]
    fn test_http_idle_timeout_rejects_invalid() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"httpIdleTimeoutMs": -1}));
        let manager = create(&dirs);

        match manager.get_http_idle_timeout_ms() {
            Err(PirError::Settings(message)) => {
                assert!(
                    message.contains("Invalid httpIdleTimeoutMs setting"),
                    "{message}"
                );
            }
            other => panic!("expected settings error, got {other:?}"),
        }
    }

    // =======================================================================
    // describe("externalEditor")
    // =======================================================================

    // Port of "should resolve editor commands by precedence".
    #[test]
    fn test_external_editor_precedence() {
        let (_lock, _guard) = EnvGuard::set(&[
            (environment::ENV_VISUAL, Some("vim")),
            (environment::ENV_EDITOR, Some("nano")),
        ]);
        assert_eq!(
            SettingsManager::in_memory(
                settings(json!({"externalEditor": "code --wait"})),
                SettingsManagerCreateOptions::default(),
            )
            .get_external_editor_command(),
            "code --wait"
        );
        assert_eq!(
            SettingsManager::in_memory(Settings::new(), SettingsManagerCreateOptions::default())
                .get_external_editor_command(),
            "vim"
        );

        std::env::remove_var(environment::ENV_VISUAL);
        std::env::set_var(environment::ENV_EDITOR, "emacs");
        assert_eq!(
            SettingsManager::in_memory(Settings::new(), SettingsManagerCreateOptions::default())
                .get_external_editor_command(),
            "emacs"
        );
    }

    // Port of "should fall back to platform defaults" (the win32/darwin/linux
    // branches collapse to a cfg!(windows) check in Rust).
    #[test]
    fn test_external_editor_platform_default() {
        let (_lock, _guard) = EnvGuard::set(&[
            (environment::ENV_VISUAL, None),
            (environment::ENV_EDITOR, None),
        ]);
        let expected = if cfg!(windows) { "notepad" } else { "nano" };
        assert_eq!(
            SettingsManager::in_memory(Settings::new(), SettingsManagerCreateOptions::default())
                .get_external_editor_command(),
            expected
        );
    }

    // =======================================================================
    // describe("outputPad")
    // =======================================================================

    // Port of "should default to 1 and persist binary values".
    #[test]
    fn test_output_pad_default_and_persist() {
        let dirs = test_dirs();
        let mut manager = create(&dirs);

        assert_eq!(manager.get_output_pad(), 1);

        manager.set_output_pad(0);

        assert_eq!(manager.get_output_pad(), 0);
        let saved = read_json(&global_path(&dirs));
        assert_eq!(saved["outputPad"], json!(0));
    }

    // Port of "should treat unsupported outputPad values as default padding".
    #[test]
    fn test_output_pad_unsupported_value_defaults() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"outputPad": 2}));

        let manager = create(&dirs);

        assert_eq!(manager.get_output_pad(), 1);
    }

    // =======================================================================
    // describe("shellCommandPrefix")
    // =======================================================================

    // Port of "should load shellCommandPrefix from settings".
    #[test]
    fn test_shell_command_prefix_loaded() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"shellCommandPrefix": "shopt -s expand_aliases"}),
        );

        let manager = create(&dirs);

        assert_eq!(
            manager.get_shell_command_prefix().as_deref(),
            Some("shopt -s expand_aliases")
        );
    }

    // Port of "should return undefined when shellCommandPrefix is not set".
    #[test]
    fn test_shell_command_prefix_undefined() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));

        let manager = create(&dirs);

        assert_eq!(manager.get_shell_command_prefix(), None);
    }

    // Port of "should preserve shellCommandPrefix when saving unrelated settings".
    #[test]
    fn test_shell_command_prefix_preserved_on_unrelated_save() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"shellCommandPrefix": "shopt -s expand_aliases"}),
        );

        let mut manager = create(&dirs);
        manager.set_theme("light");

        let saved = read_json(&global_path(&dirs));
        assert_eq!(
            saved["shellCommandPrefix"],
            json!("shopt -s expand_aliases")
        );
        assert_eq!(saved["theme"], json!("light"));
    }

    // =======================================================================
    // describe("getSessionDir")
    // =======================================================================

    // Port of "should return undefined when not set".
    #[test]
    fn test_session_dir_undefined() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));
        let manager = create(&dirs);
        assert_eq!(manager.get_session_dir(), None);
    }

    // Port of "should return global sessionDir".
    #[test]
    fn test_session_dir_global() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"sessionDir": "/tmp/sessions"}));
        let manager = create(&dirs);
        assert_eq!(manager.get_session_dir().as_deref(), Some("/tmp/sessions"));
    }

    // Port of "should return project sessionDir, overriding global".
    #[test]
    fn test_session_dir_project_overrides() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"sessionDir": "/global/sessions"}),
        );
        write_json(&project_path(&dirs), json!({"sessionDir": "./sessions"}));
        let manager = create(&dirs);
        assert_eq!(manager.get_session_dir().as_deref(), Some("./sessions"));
    }

    // Port of "should expand ~ in sessionDir".
    #[test]
    fn test_session_dir_expands_tilde() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"sessionDir": "~/sessions"}));
        let manager = create(&dirs);
        if let Some(home) = std::env::var_os("HOME") {
            let expected = PathBuf::from(home).join("sessions");
            assert_eq!(
                manager.get_session_dir().as_deref(),
                Some(expected.to_string_lossy().as_ref())
            );
        }
    }

    // =======================================================================
    // describe("getShellPath")
    // =======================================================================

    // Port of "should return undefined when not set".
    #[test]
    fn test_shell_path_undefined() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));
        let manager = create(&dirs);
        assert_eq!(manager.get_shell_path(), None);
    }

    // Port of "should return an absolute shellPath unchanged".
    #[test]
    fn test_shell_path_absolute() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"shellPath": "/bin/zsh"}));
        let manager = create(&dirs);
        assert_eq!(manager.get_shell_path().as_deref(), Some("/bin/zsh"));
    }

    // Port of "should expand ~ in shellPath".
    #[test]
    fn test_shell_path_expands_tilde() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"shellPath": "~/.local/bin/agent-shell-sandbox"}),
        );
        let manager = create(&dirs);
        if let Some(home) = std::env::var_os("HOME") {
            let expected = PathBuf::from(home).join(".local/bin/agent-shell-sandbox");
            assert_eq!(
                manager.get_shell_path().as_deref(),
                Some(expected.to_string_lossy().as_ref())
            );
        }
    }

    // Port of "should expand a bare ~ in shellPath".
    #[test]
    fn test_shell_path_bare_tilde() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"shellPath": "~"}));
        let manager = create(&dirs);
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                manager.get_shell_path().as_deref(),
                Some(PathBuf::from(home).to_string_lossy().as_ref())
            );
        }
    }

    // =======================================================================
    // deepMergeSettings unit tests (settings-manager.ts:132-160 semantics;
    // the upstream comment claims recursion — the code does not recurse)
    // =======================================================================

    #[test]
    fn test_deep_merge_top_level_union_and_scalar_replace() {
        let base = settings(json!({"theme": "dark", "defaultModel": "a"}));
        let overrides = settings(json!({"theme": "light", "defaultProvider": "p"}));
        let merged = deep_merge_settings(&base, &overrides);
        assert_eq!(
            merged.as_map(),
            &json!({"theme": "light", "defaultModel": "a", "defaultProvider": "p"})
                .as_object()
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn test_deep_merge_nested_objects_single_level_shallow() {
        // Depth 1: override sub-keys win, base-only sub-keys survive.
        let base = settings(json!({"compaction": {"enabled": false, "reserveTokens": 100}}));
        let overrides = settings(json!({"compaction": {"reserveTokens": 200}}));
        let merged = deep_merge_settings(&base, &overrides);
        assert_eq!(
            merged.as_map()["compaction"],
            json!({"enabled": false, "reserveTokens": 200})
        );
    }

    #[test]
    fn test_deep_merge_depth_two_replaces_wholesale() {
        // Depth >= 2: retry.provider is replaced wholesale, not merged.
        let base = settings(
            json!({"retry": {"enabled": true, "provider": {"timeoutMs": 1000, "maxRetries": 2}}}),
        );
        let overrides = settings(json!({"retry": {"provider": {"maxRetries": 5}}}));
        let merged = deep_merge_settings(&base, &overrides);
        assert_eq!(
            merged.as_map()["retry"],
            json!({"enabled": true, "provider": {"maxRetries": 5}})
        );
    }

    #[test]
    fn test_deep_merge_arrays_and_null_replace_wholesale() {
        let base = settings(json!({"extensions": ["/a.ts"], "theme": "dark"}));
        let overrides = settings(json!({"extensions": ["/b.ts"], "theme": null}));
        let merged = deep_merge_settings(&base, &overrides);
        assert_eq!(merged.as_map()["extensions"], json!(["/b.ts"]));
        assert_eq!(merged.as_map()["theme"], Value::Null);

        // An object override replaces a non-object base wholesale.
        let base = settings(json!({"compaction": "oops"}));
        let overrides = settings(json!({"compaction": {"enabled": false}}));
        let merged = deep_merge_settings(&base, &overrides);
        assert_eq!(merged.as_map()["compaction"], json!({"enabled": false}));
    }

    // =======================================================================
    // migrateSettings unit tests (settings-manager.ts:381-440)
    // =======================================================================

    #[test]
    fn test_migrate_queue_mode_to_steering_mode() {
        let mut map = json!({"queueMode": "all"}).as_object().unwrap().clone();
        migrate_settings(&mut map);
        assert_eq!(map["steeringMode"], json!("all"));
        assert!(!map.contains_key("queueMode"));

        // Existing steeringMode wins; queueMode is kept verbatim upstream
        // (the condition requires steeringMode absent).
        let mut map = json!({"queueMode": "all", "steeringMode": "one-at-a-time"})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert_eq!(map["steeringMode"], json!("one-at-a-time"));
        assert_eq!(map["queueMode"], json!("all"));
    }

    #[test]
    fn test_migrate_websockets_bool_to_transport() {
        let mut map = json!({"websockets": true}).as_object().unwrap().clone();
        migrate_settings(&mut map);
        assert_eq!(map["transport"], json!("websocket"));
        assert!(!map.contains_key("websockets"));

        let mut map = json!({"websockets": false}).as_object().unwrap().clone();
        migrate_settings(&mut map);
        assert_eq!(map["transport"], json!("sse"));

        // Non-boolean websockets is left alone; existing transport wins.
        let mut map = json!({"websockets": "yes"}).as_object().unwrap().clone();
        migrate_settings(&mut map);
        assert_eq!(map["websockets"], json!("yes"));
        assert!(!map.contains_key("transport"));

        let mut map = json!({"websockets": true, "transport": "auto"})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert_eq!(map["transport"], json!("auto"));
        assert_eq!(map["websockets"], json!(true));
    }

    #[test]
    fn test_migrate_legacy_skills_object() {
        // customDirectories promotes to the skills array;
        // enableSkillCommands lifts to the top level.
        let mut map =
            json!({"skills": {"enableSkillCommands": false, "customDirectories": ["/a", "/b"]}})
                .as_object()
                .unwrap()
                .clone();
        migrate_settings(&mut map);
        assert_eq!(map["skills"], json!(["/a", "/b"]));
        assert_eq!(map["enableSkillCommands"], json!(false));

        // Empty customDirectories deletes the skills key.
        let mut map = json!({"skills": {"customDirectories": []}})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert!(!map.contains_key("skills"));

        // Missing customDirectories deletes the skills key.
        let mut map = json!({"skills": {"enableSkillCommands": true}})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert!(!map.contains_key("skills"));
        assert_eq!(map["enableSkillCommands"], json!(true));

        // Top-level enableSkillCommands wins over the legacy nested one.
        let mut map =
            json!({"skills": {"enableSkillCommands": false}, "enableSkillCommands": true})
                .as_object()
                .unwrap()
                .clone();
        migrate_settings(&mut map);
        assert_eq!(map["enableSkillCommands"], json!(true));
    }

    #[test]
    fn test_migrate_retry_max_delay_ms() {
        // Moved into retry.provider.maxRetryDelayMs.
        let mut map = json!({"retry": {"maxDelayMs": 5000}})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert_eq!(map["retry"], json!({"provider": {"maxRetryDelayMs": 5000}}));

        // Existing provider keys survive; null maxRetryDelayMs is overwritten.
        let mut map = json!({"retry": {"maxDelayMs": 5000, "provider": {"timeoutMs": 100, "maxRetryDelayMs": null}}})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert_eq!(
            map["retry"],
            json!({"provider": {"timeoutMs": 100, "maxRetryDelayMs": 5000}})
        );

        // Existing provider.maxRetryDelayMs wins; maxDelayMs is still deleted
        // (the delete is unconditional upstream).
        let mut map = json!({"retry": {"maxDelayMs": 5000, "provider": {"maxRetryDelayMs": 9000}}})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert_eq!(map["retry"], json!({"provider": {"maxRetryDelayMs": 9000}}));

        // Non-number maxDelayMs is deleted without migration.
        let mut map = json!({"retry": {"maxDelayMs": "5000"}})
            .as_object()
            .unwrap()
            .clone();
        migrate_settings(&mut map);
        assert_eq!(map["retry"], json!({}));
    }

    // =======================================================================
    // Field-level write persistence (settings-manager.ts:578-607)
    // =======================================================================

    /// Nested objects write per modified sub-key: sub-keys added externally
    /// on disk survive a nested write (settings-manager.ts:591-599).
    #[test]
    fn test_nested_persistence_merges_per_modified_key() {
        let dirs = test_dirs();
        write_json(
            &global_path(&dirs),
            json!({"terminal": {"showImages": true}}),
        );

        let mut manager = create(&dirs);

        // External edit adds another terminal sub-key.
        let mut current = read_json(&global_path(&dirs));
        current["terminal"]["clearOnShrink"] = json!(true);
        write_json(&global_path(&dirs), current);

        // Session modifies only terminal.showImages.
        manager.set_show_images(false);

        let saved = read_json(&global_path(&dirs));
        assert_eq!(saved["terminal"]["showImages"], json!(false));
        assert_eq!(saved["terminal"]["clearOnShrink"], json!(true));
    }

    /// A disk-side legacy key is migrated when read back during a write
    /// (settings-manager.ts:586).
    #[test]
    fn test_write_remigrates_disk_content() {
        let dirs = test_dirs();
        write_json(&global_path(&dirs), json!({"theme": "dark"}));

        let mut manager = create(&dirs);

        // External edit reintroduces a legacy key.
        write_json(
            &global_path(&dirs),
            json!({"theme": "dark", "queueMode": "all"}),
        );

        manager.set_quiet_startup(true);

        let saved = read_json(&global_path(&dirs));
        assert_eq!(saved["steeringMode"], json!("all"));
        assert!(!saved.as_object().unwrap().contains_key("queueMode"));
    }

    /// Default values never hit the disk: a fresh write contains only the
    /// key the session actually set (settings-manager.ts:578-607).
    #[test]
    fn test_defaults_are_not_persisted() {
        let dirs = test_dirs();
        let mut manager = create(&dirs);

        manager.set_theme("light");

        let saved = read_json(&global_path(&dirs));
        let map = saved.as_object().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(saved["theme"], json!("light"));
    }

    /// Serialized output is 2-space pretty JSON without a trailing newline
    /// (`JSON.stringify(obj, null, 2)`).
    #[test]
    fn test_serialization_format_two_space_no_trailing_newline() {
        let dirs = test_dirs();
        let mut manager = create(&dirs);
        manager.set_theme("light");

        let raw = std::fs::read_to_string(global_path(&dirs)).unwrap();
        assert_eq!(raw, "{\n  \"theme\": \"light\"\n}");
    }

    /// `setEnableAnalytics` generates a trackingId (UUID v4 shape) on first
    /// opt-in only (settings-manager.ts:958-967).
    #[test]
    fn test_enable_analytics_generates_tracking_id_once() {
        let dirs = test_dirs();
        let mut manager = create(&dirs);

        manager.set_enable_analytics(true);
        let first = manager.get_tracking_id().expect("trackingId generated");
        let uuid_shape = first.len() == 36 && first.chars().filter(|c| *c == '-').count() == 4;
        assert!(uuid_shape, "not a UUID: {first}");

        manager.set_enable_analytics(false);
        manager.set_enable_analytics(true);
        assert_eq!(manager.get_tracking_id().as_deref(), Some(first.as_str()));
    }

    /// `InMemorySettingsStorage` round-trips writes within a manager
    /// (settings-manager.ts:257-272, 343-348).
    #[test]
    fn test_in_memory_storage_round_trip() {
        let mut manager = SettingsManager::in_memory(
            settings(json!({"theme": "dark"})),
            SettingsManagerCreateOptions::default(),
        );
        assert_eq!(manager.get_theme().as_deref(), Some("dark"));

        manager.set_theme("light");
        assert_eq!(manager.get_theme().as_deref(), Some("light"));

        manager.reload();
        assert_eq!(manager.get_theme().as_deref(), Some("light"));
    }

    /// Getter defaults on an empty settings file (docs/settings.md).
    #[test]
    fn test_getter_defaults() {
        let dirs = test_dirs();
        let manager = create(&dirs);

        assert_eq!(manager.get_steering_mode(), QueueMode::OneAtATime);
        assert_eq!(manager.get_follow_up_mode(), QueueMode::OneAtATime);
        assert_eq!(manager.get_transport(), Transport::Auto);
        assert!(manager.get_compaction_enabled());
        assert_eq!(manager.get_compaction_reserve_tokens(), 16384);
        assert_eq!(manager.get_compaction_keep_recent_tokens(), 20000);
        assert_eq!(
            manager.get_branch_summary_settings(),
            BranchSummaryConfig {
                reserve_tokens: 16384,
                skip_prompt: false,
            }
        );
        assert_eq!(
            manager.get_retry_settings(),
            RetryConfig {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 2000,
            }
        );
        assert_eq!(
            manager.get_provider_retry_settings(),
            ProviderRetryConfig {
                timeout_ms: None,
                max_retries: None,
                max_retry_delay_ms: 60000,
            }
        );
        assert!(!manager.get_hide_thinking_block());
        assert!(!manager.get_show_cache_miss_notices());
        assert!(!manager.get_quiet_startup());
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Ask
        );
        assert!(!manager.get_collapse_changelog());
        assert!(manager.get_enable_install_telemetry());
        assert!(!manager.get_enable_analytics());
        assert_eq!(manager.get_tracking_id(), None);
        assert!(manager.get_enable_skill_commands());
        assert!(manager.get_show_images());
        assert_eq!(manager.get_image_width_cells(), 60);
        assert!(!manager.get_show_terminal_progress());
        assert!(manager.get_image_auto_resize());
        assert!(!manager.get_block_images());
        assert_eq!(manager.get_double_escape_action(), DoubleEscapeAction::Tree);
        assert_eq!(manager.get_tree_filter_mode(), TreeFilterMode::Default);
        assert_eq!(manager.get_editor_padding_x(), 0);
        assert_eq!(manager.get_output_pad(), 1);
        assert_eq!(manager.get_autocomplete_max_visible(), 5);
        assert_eq!(manager.get_code_block_indent(), "  ");
        assert_eq!(manager.get_websocket_connect_timeout_ms().unwrap(), None);
    }
}
