use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use jsonschema::Validator;

use rpi_ai::models_json::ModelsJson;
use rpi_ai::types::{ModelThinkingLevel, Transport};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::keybindings::{self, KeyBindingValue};
use crate::core::settings_manager::{
    BranchSummarySettings, CacheWarmingMode, CodemodeSettings, CompactionSettings,
    DefaultProjectTrust, DoubleEscapeAction, FullscreenExitOutput, ImageSettings, MarkdownSettings,
    PackageSource, QuietStartupSetting, RetrySettings, SessionShareDestination, SkillsSetting,
    TerminalSettings, ThinkingBudgetsSettings, TimeoutSetting, TreeFilterMode, UsageDocument,
    WarningSettings, WheelScrollLinesSetting,
};
use crate::core::themes::{ALLOWED_COLOR_KEYS, REQUIRED_COLOR_KEYS, ThemeJson};

pub const SCHEMA_COMMENT: &str = "Generated from the rpi typed configuration models by scripts/sync-config-schemas.py. Do not edit it manually; run the script to regenerate.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum QueueModeDocument {
    All,
    OneAtATime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TuiModeDocument {
    Regular,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScrollbarModeDocument {
    Hidden,
    Auto,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusLineType {
    Command,
    Items,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusLinePlacement {
    Replace,
    Widget,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusLineNumberFormat {
    Compact,
    Exact,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct StatusLineLiveTokensOptions {
    pub refresh_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StatusLineLiveTokens {
    Enabled(bool),
    Options(StatusLineLiveTokensOptions),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct StatusLineDocument {
    #[serde(rename = "type")]
    pub mode: Option<StatusLineType>,
    pub command: Option<String>,
    pub items: Option<Vec<String>>,
    pub placement: Option<StatusLinePlacement>,
    pub timeout_ms: Option<u64>,
    pub padding: Option<u64>,
    pub refresh_ms: Option<u64>,
    pub live_tokens: Option<StatusLineLiveTokens>,
    pub separator: Option<String>,
    pub refresh_interval: Option<u64>,
    pub use_colors: Option<bool>,
    pub number_format: Option<StatusLineNumberFormat>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("additionalProperties" = true))]
pub struct SettingsDocument {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub last_changelog_version: Option<String>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<ModelThinkingLevel>,
    pub model_thinking_levels: Option<BTreeMap<String, ModelThinkingLevel>>,
    pub transport: Option<Transport>,
    /// Destination used by /share. "auto" and "gist" create a GitHub gist; "radius" is recognized but unsupported and fails explicitly.
    pub session_share_destination: Option<SessionShareDestination>,
    pub steering_mode: Option<QueueModeDocument>,
    pub follow_up_mode: Option<QueueModeDocument>,
    pub theme: Option<String>,
    pub compaction: Option<CompactionSettings>,
    pub branch_summary: Option<BranchSummarySettings>,
    pub retry: Option<RetrySettings>,
    pub hide_thinking_block: Option<bool>,
    pub show_cache_miss_notices: Option<bool>,
    pub external_editor: Option<String>,
    pub shell_path: Option<String>,
    pub quiet_startup: Option<QuietStartupSetting>,
    pub default_project_trust: Option<DefaultProjectTrust>,
    pub shell_command_prefix: Option<String>,
    pub npm_command: Option<Vec<String>>,
    pub collapse_changelog: Option<bool>,
    pub enable_install_telemetry: Option<bool>,
    pub enable_analytics: Option<bool>,
    pub tracking_id: Option<String>,
    pub packages: Option<Vec<PackageSource>>,
    pub extensions: Option<Vec<String>>,
    pub device_id: Option<String>,
    pub skills: Option<SkillsSetting>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
    pub enable_skill_commands: Option<bool>,
    pub terminal: Option<TerminalSettings>,
    pub images: Option<ImageSettings>,
    pub enabled_models: Option<Vec<String>>,
    pub default_tools: Option<Vec<String>>,
    pub double_escape_action: Option<DoubleEscapeAction>,
    pub tree_filter_mode: Option<TreeFilterMode>,
    pub thinking_budgets: Option<ThinkingBudgetsSettings>,
    pub editor_padding_x: Option<u64>,
    pub output_pad: Option<u8>,
    pub autocomplete_max_visible: Option<u64>,
    pub show_hardware_cursor: Option<bool>,
    pub markdown: Option<MarkdownSettings>,
    pub warnings: Option<WarningSettings>,
    pub codemode: Option<CodemodeSettings>,
    pub session_dir: Option<String>,
    pub http_proxy: Option<String>,
    pub http_idle_timeout_ms: Option<TimeoutSetting>,
    pub cache_warming: Option<CacheWarmingMode>,
    pub websocket_connect_timeout_ms: Option<TimeoutSetting>,
    pub tui_mode: Option<TuiModeDocument>,
    pub fullscreen_exit_output: Option<FullscreenExitOutput>,
    pub fullscreen_scrollbar: Option<ScrollbarModeDocument>,
    pub fullscreen_copy_on_select: Option<bool>,
    pub fullscreen_wheel_scroll_lines: Option<WheelScrollLinesSetting>,
    pub queue_mode: Option<QueueModeDocument>,
    pub websockets: Option<bool>,
    pub usage: Option<UsageDocument>,
    pub status_line: Option<StatusLineDocument>,
    pub version_check_url: Option<String>,
    pub telemetry_url: Option<String>,
    pub model_catalog_url: Option<String>,
    pub subagents: Option<Map<String, Value>>,
}

fn normalize_js_number_fields(value: &mut Value) {
    const FLOORED_FIELDS: &[&[&str]] = &[
        &["branchSummary", "reserveTokens"],
        &["retry", "maxRetries"],
        &["retry", "baseDelayMs"],
        &["retry", "maxAgentDelayMs"],
        &["retry", "maxDelayMs"],
        &["retry", "provider", "timeoutMs"],
        &["retry", "provider", "maxRetries"],
        &["retry", "provider", "maxRetryDelayMs"],
        &["terminal", "imageWidthCells"],
        &["codemode", "inlineBudget"],
        &["usage", "timeoutMs"],
        &["fullscreenWheelScrollLines"],
        &["editorPaddingX"],
        &["autocompleteMaxVisible"],
    ];
    for path in FLOORED_FIELDS {
        let Some(field) = path
            .iter()
            .try_fold(&mut *value, |current, segment| current.get_mut(*segment))
        else {
            continue;
        };
        if let Some(number) = field.as_f64()
            && number.is_finite()
        {
            *field = Value::from(number.floor() as u64);
        }
    }
}

pub fn validate_settings_document(value: &Value) -> Result<SettingsDocument, String> {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR
        .get_or_init(|| Validator::new(&settings_schema()).expect("settings schema validator"))
        .validate(value)
        .map_err(|error| error.to_string())?;
    decode_settings_document(value)
}

pub fn validate_runtime_settings_document(value: &Value) -> Result<SettingsDocument, String> {
    static VALIDATORS: OnceLock<(Validator, Validator)> = OnceLock::new();
    let (usage, status_line) = VALIDATORS.get_or_init(|| {
        let schema = settings_schema();
        let definitions = schema["$defs"].clone();
        let scoped = |name: &str| {
            Validator::new(&serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$defs": definitions.clone(),
                "$ref": format!("#/$defs/{name}"),
            }))
            .expect("scoped settings schema validator")
        };
        (scoped("UsageDocument"), scoped("StatusLineDocument"))
    });
    for (field, validator) in [("usage", usage), ("statusLine", status_line)] {
        if let Some(scoped) = value.get(field) {
            validator
                .validate(scoped)
                .map_err(|error| format!("{field}: {error}"))?;
        }
    }
    decode_settings_document(value)
}

fn decode_settings_document(value: &Value) -> Result<SettingsDocument, String> {
    let mut normalized = value.clone();
    normalize_js_number_fields(&mut normalized);
    let document: SettingsDocument =
        serde_json::from_value(normalized).map_err(|error| error.to_string())?;
    if let Some(usage) = &document.usage {
        for (name, provider) in &usage.providers {
            if name.is_empty() {
                return Err("usage.providers keys must not be empty".to_string());
            }
            let script = match provider {
                crate::core::settings_manager::UsageProviderSetting::Script(script) => script,
                crate::core::settings_manager::UsageProviderSetting::Detailed(config) => {
                    &config.script
                }
            };
            if script.is_empty() {
                return Err(format!("usage.providers.{name}.script must not be empty"));
            }
        }
    }
    Ok(document)
}

pub fn validate_keybindings_document(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| "top level is not a JSON object".to_string())?;
    let mut fields = object;
    if let Some(schema) = fields.remove("$schema")
        && !schema.is_string()
    {
        return Err("$schema must be a string".to_string());
    }
    let _: BTreeMap<String, KeyBindingValue> =
        serde_json::from_value(Value::Object(fields)).map_err(|error| error.to_string())?;
    Ok(())
}

fn with_public_metadata(mut schema: Value, name: &str) -> Value {
    schema["$id"] = format!("https://revpi.dev/schemas/{name}").into();
    schema["$comment"] = SCHEMA_COMMENT.into();
    schema
}

fn remove_nullable_options(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(options)) = map.get_mut("anyOf") {
                options.retain(|option| option["type"] != "null");
            }
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|kind| kind != "null");
            }
            for child in map.values_mut() {
                remove_nullable_options(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                remove_nullable_options(child);
            }
        }
        _ => {}
    }
}

fn use_js_numbers(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("integer")
                || map
                    .get("type")
                    .and_then(Value::as_array)
                    .is_some_and(|types| types.iter().any(|kind| kind == "integer"))
            {
                map.insert("type".to_string(), "number".into());
            }
            for child in map.values_mut() {
                use_js_numbers(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                use_js_numbers(child);
            }
        }
        _ => {}
    }
}

pub fn settings_schema() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(SettingsDocument))
        .expect("serialize settings schema");
    remove_nullable_options(&mut schema);
    use_js_numbers(&mut schema);
    for definition in ["CompactionSettings", "CompactionModelOverride"] {
        for property in ["reserveTokens", "keepRecentTokens"] {
            let schema = &mut schema["$defs"][definition]["properties"][property];
            if !schema.is_null() {
                schema["type"] = "integer".into();
                schema["maximum"] = serde_json::json!(9_007_199_254_740_991_u64);
            }
        }
    }
    for definition in [
        "StatusLineDocument",
        "StatusLineLiveTokensOptions",
        "ThinkingBudgetsSettings",
    ] {
        if let Some(properties) = schema["$defs"][definition]["properties"].as_object_mut() {
            for property in properties.values_mut() {
                if property["type"] == "number" {
                    property["type"] = "integer".into();
                }
            }
        }
    }
    schema["$defs"]["UsageDocument"]["properties"]["refreshMs"]["type"] = "integer".into();
    let status_line = &mut schema["$defs"]["StatusLineDocument"];
    status_line["required"] = serde_json::json!(["type"]);
    status_line["properties"]["type"] = serde_json::json!({"$ref": "#/$defs/StatusLineType"});
    status_line["properties"]["command"] =
        serde_json::json!({"type": "string", "minLength": 1, "pattern": "\\S"});
    status_line["anyOf"] = serde_json::json!([
        {"properties": {"type": {"const": "command"}}, "required": ["command"]},
        {"properties": {"type": {"const": "items"}}}
    ]);
    schema["$defs"]["UsageDocument"]["properties"]["providers"]["propertyNames"] =
        serde_json::json!({"minLength": 1});
    schema["properties"]["outputPad"] = serde_json::json!({"type": "integer", "enum": [0, 1]});
    with_public_metadata(schema, "settings.schema.json")
}

pub fn models_schema() -> Value {
    with_public_metadata(ModelsJson::schema_document(), "models.schema.json")
}

pub fn keybindings_schema() -> Value {
    let mut properties = Map::new();
    properties.insert(
        "$schema".to_string(),
        serde_json::json!({"type": "string", "description": "JSON Schema reference."}),
    );
    for (id, definition) in keybindings::keybinding_definitions() {
        properties.insert(
            id.clone(),
            serde_json::json!({
                "$ref": "#/$defs/KeybindingValue",
                "description": definition.description,
            }),
        );
    }
    let key_value = serde_json::to_value(schemars::schema_for!(KeyBindingValue))
        .expect("serialize keybinding value");
    with_public_metadata(
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": properties,
            "additionalProperties": {"$ref": "#/$defs/KeybindingValue"},
            "$defs": {"KeybindingValue": key_value},
        }),
        "keybindings.schema.json",
    )
}

pub fn theme_schema() -> Value {
    let mut schema = with_public_metadata(
        serde_json::to_value(schemars::schema_for!(ThemeJson)).expect("serialize theme schema"),
        "theme.schema.json",
    );
    schema["title"] = "Rpi Coding Agent Theme".into();
    schema["description"] = "Theme schema for the rpi coding agent".into();
    schema["additionalProperties"] = false.into();
    schema["required"] = serde_json::json!(["name", "colors"]);
    let mut color_properties = Map::new();
    for key in ALLOWED_COLOR_KEYS {
        color_properties.insert(
            (*key).to_string(),
            serde_json::json!({"$ref": "#/$defs/ColorValue"}),
        );
    }
    schema["properties"]["colors"]["properties"] = Value::Object(color_properties);
    schema["properties"]["colors"]["required"] =
        serde_json::to_value(REQUIRED_COLOR_KEYS).expect("required color keys");
    schema["properties"]["colors"]["additionalProperties"] = false.into();
    schema["$defs"]["ThemeExport"]["additionalProperties"] = false.into();
    schema
}

pub fn config_schemas() -> HashMap<&'static str, Value> {
    HashMap::from([
        ("settings.schema.json", settings_schema()),
        ("models.schema.json", models_schema()),
        ("keybindings.schema.json", keybindings_schema()),
        ("theme.schema.json", theme_schema()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_schema_contains_rpi_own_typed_keys() {
        let schema = settings_schema();
        assert!(schema["properties"]["usage"].is_object());
        assert!(schema["properties"]["statusLine"].is_object());
        assert!(schema["properties"]["versionCheckUrl"].is_object());
    }

    #[test]
    fn invalid_usage_provider_shapes_are_rejected() {
        let error = validate_settings_document(&serde_json::json!({
            "usage": {"providers": {"deepseek": {"script": 42}}}
        }))
        .expect_err("script must be a string");
        assert!(!error.is_empty(), "{error}");
    }

    #[test]
    fn keybindings_schema_uses_the_loader_registry() {
        let schema = keybindings_schema();
        let properties = schema["properties"].as_object().expect("properties");
        assert_eq!(
            properties.len(),
            keybindings::keybinding_definitions().len() + 1
        );
        assert!(properties.contains_key("tui.editor.cursorUp"));
    }
}
