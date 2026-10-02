//! The `codemode` tool definition and its `prepareLoadout` hook. Port of
//! `packages/coding-agent/src/extensions/codemode/tool.ts` @ a13d35a74.
//!
//! The tool is registered inactive (`exposure: model-only`,
//! `defaultActive: false`); enabling it is the `defaultTools`/`--tools`
//! governance surface owned by V16-13.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::types::{
    ToolDefinition, ToolExecuteRequest, ToolExposure, ToolLoadout, ToolLoadoutChanges,
    ToolNamespace,
};
use serde_json::{Value, json};

use crate::extensions::codemode::description::{
    CodemodeDescriptionOptions, create_codemode_description, describe_script_call,
};
use crate::extensions::codemode::execute::{CodemodeToolOptions, execute_codemode};

pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Custom entry type holding one script's `store()` writes
/// (`CODEMODE_STORE_ENTRY_TYPE`).
pub const CODEMODE_STORE_ENTRY_TYPE: &str = "codemode-store";

/// Default for `inlineBudget`, in estimated tokens.
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;

/// `codemode.mode` (settings-manager.ts:103-108; key registration/validation
/// is V16-13's FR-D, consumed here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodemodeMode {
    #[default]
    On,
    Only,
}

/// The consumed settings surface (`codemode.mode` / `codemode.inlineBudget`).
#[derive(Debug, Clone, Default)]
pub struct CodemodeSettings {
    pub mode: CodemodeMode,
    /// `codemode.inlineBudget`: finite, non-negative; `None` = unset.
    pub inline_budget: Option<u64>,
}

/// Reads the current settings (the app wires this to the session's settings
/// manager; V16-13 registers the keys).
pub type CodemodeSettingsFn = Arc<dyn Fn() -> CodemodeSettings + Send + Sync>;

/// Consumption-only reader for `codemode.mode` / `codemode.inlineBudget`
/// (key registration/validation is V16-13's FR-D). Project values override
/// global ones per key; malformed values fall back to the defaults
/// (`readMode`/`readInlineBudget`, codemode/index.ts:20-30).
pub fn settings_from_manager(
    manager: &crate::core::settings_manager::SettingsManager,
) -> CodemodeSettings {
    let lookup = |key: &str| -> Option<serde_json::Value> {
        let global = manager.get_global_settings();
        let project = manager.get_project_settings();
        let from = |settings: &crate::core::settings_manager::Settings| {
            settings
                .as_map()
                .get("codemode")
                .and_then(serde_json::Value::as_object)
                .and_then(|object| object.get(key))
                .cloned()
        };
        from(&project).or_else(|| from(&global))
    };
    let mode = match lookup("mode").as_ref().and_then(serde_json::Value::as_str) {
        Some("only") => CodemodeMode::Only,
        _ => CodemodeMode::On,
    };
    let inline_budget = lookup("inlineBudget")
        .as_ref()
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.floor() as u64);
    CodemodeSettings {
        mode,
        inline_budget,
    }
}

/// `ToolInfo` view of one registered tool (`agent-session.ts:1465-1476`).
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub output_schema: Option<Value>,
    pub exposure: ToolExposure,
    pub namespace: Option<ToolNamespace>,
}

/// Parse `pi.getAllTools()` JSON into [`ToolInfo`]s, skipping malformed
/// entries.
pub fn parse_tool_infos(api: &ExtensionApi) -> Vec<ToolInfo> {
    api.get_all_tools()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tool| {
            let object = tool.as_object()?;
            let name = object.get("name")?.as_str()?.to_owned();
            Some(ToolInfo {
                name,
                description: object
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                parameters: object.get("parameters").cloned().unwrap_or(Value::Null),
                output_schema: object
                    .get("outputSchema")
                    .filter(|value| !value.is_null())
                    .cloned(),
                exposure: object
                    .get("exposure")
                    .and_then(|value| serde_json::from_value::<ToolExposure>(value.clone()).ok())
                    .unwrap_or_default(),
                namespace: object
                    .get("namespace")
                    .filter(|value| !value.is_null())
                    .and_then(|value| serde_json::from_value::<ToolNamespace>(value.clone()).ok()),
            })
        })
        .collect()
}

/// Convert a registered tool into the declaration-only view scripts see.
/// Tools without an output schema resolve to their text output.
pub fn to_codemode_declaration(tool: &ToolInfo) -> rpi_codemode::CodemodeToolInfo {
    rpi_codemode::CodemodeToolInfo {
        name: tool.name.clone(),
        description: Some(tool.description.clone()),
        input_schema: Some(tool.parameters.clone()),
        output_schema: Some(
            tool.output_schema
                .clone()
                .unwrap_or_else(|| json!({ "type": "string" })),
        ),
        signature: None,
    }
}

/// Tools a script may call: every given tool except the codemode tool
/// itself (tool.ts:169-171).
pub fn get_codemode_callable_tools(tools: &[ToolInfo]) -> Vec<ToolInfo> {
    tools
        .iter()
        .filter(|tool| tool.name != CODEMODE_TOOL_NAME)
        .cloned()
        .collect()
}

/// The callable set of the current session (`ctx.tools` upstream): active
/// `direct` tools plus every `codemode`/`deferred` tool.
pub fn callable_tools(api: &ExtensionApi) -> Vec<ToolInfo> {
    let active: HashSet<String> = api
        .get_active_tools()
        .unwrap_or_default()
        .into_iter()
        .collect();
    get_codemode_callable_tools(&parse_tool_infos(api))
        .into_iter()
        .filter(|tool| match tool.exposure {
            ToolExposure::Codemode | ToolExposure::Deferred => true,
            ToolExposure::Direct => active.contains(&tool.name),
            _ => false,
        })
        .collect()
}

/// The loadout conversion of [`ToolInfo`] (the hook sees
/// [`rpi_ext_host::types::ToolLoadoutEntry`]).
fn loadout_tool_info(entry: &rpi_ext_host::types::ToolLoadoutEntry) -> ToolInfo {
    ToolInfo {
        name: entry.name.clone(),
        description: entry.description.clone(),
        parameters: entry.parameters.clone(),
        output_schema: entry.output_schema.clone(),
        exposure: ToolExposure::Direct,
        namespace: None,
    }
}

/// How the codemode tool presents tools that are both declared and callable
/// from scripts (`prepareCodemodeLoadout`, tool.ts:329-363).
fn prepare_codemode_loadout(
    loadout: &ToolLoadout,
    options: &CodemodeToolOptions,
) -> ToolLoadoutChanges {
    let mode = (options.get_mode)();
    let is_direct = |name: &str| loadout.get_exposure(name) == ToolExposure::Direct;
    let callable: Vec<ToolInfo> = loadout
        .callable
        .iter()
        .map(loadout_tool_info)
        .filter(|tool| tool.name != CODEMODE_TOOL_NAME)
        .collect();
    let callable_names: HashSet<&str> = callable.iter().map(|tool| tool.name.as_str()).collect();
    let mut descriptions: HashMap<String, String> = HashMap::new();
    if mode == CodemodeMode::On {
        for tool in &loadout.declared {
            if callable_names.contains(tool.name.as_str()) {
                let info = loadout_tool_info(tool);
                descriptions.insert(tool.name.clone(), describe_script_call(&info));
            }
        }
    }
    let listed: Vec<ToolInfo> = if mode == CodemodeMode::Only {
        callable.clone()
    } else {
        callable
            .iter()
            .filter(|tool| !is_direct(&tool.name))
            .cloned()
            .collect()
    };
    let mut namespaces: HashMap<String, ToolNamespace> = HashMap::new();
    for tool in &listed {
        if let Some(namespace) = loadout.get_namespace(&tool.name) {
            namespaces.insert(tool.name.clone(), namespace.clone());
        }
    }
    let deferred: HashSet<String> = listed
        .iter()
        .filter(|tool| loadout.get_exposure(&tool.name) == ToolExposure::Deferred)
        .map(|tool| tool.name.clone())
        .collect();
    descriptions.insert(
        CODEMODE_TOOL_NAME.to_owned(),
        create_codemode_description(
            &listed,
            &CodemodeDescriptionOptions {
                models: options.models,
                namespaces,
                deferred,
                inline_budget: Some(
                    (options.get_inline_budget)()
                        .map(|budget| budget as usize)
                        .unwrap_or(DEFAULT_CODEMODE_INLINE_BUDGET),
                ),
            },
        ),
    );
    let declared_names: HashSet<&str> = loadout
        .declared
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    let hidden_declarations = if mode == CodemodeMode::Only {
        callable
            .iter()
            .filter(|tool| is_direct(&tool.name) && declared_names.contains(tool.name.as_str()))
            .map(|tool| tool.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    ToolLoadoutChanges {
        descriptions: Some(descriptions),
        hidden_declarations: Some(hidden_declarations),
    }
}

/// `codemodeSchema` (tool.ts:87-91).
pub fn codemode_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "description": "Raw JavaScript source." },
        },
        "required": ["code"],
        "additionalProperties": false,
    })
}

/// `createCodemodeToolDefinition` (tool.ts:365-404).
pub fn create_codemode_tool_definition(
    api: ExtensionApi,
    model_runtime: Arc<crate::core::model_runtime::ModelRuntime>,
    settings: CodemodeSettingsFn,
) -> ToolDefinition {
    let options = CodemodeToolOptions {
        api,
        model_runtime,
        models: true,
        get_mode: Arc::new({
            let settings = settings.clone();
            move || settings().mode
        }),
        get_inline_budget: Arc::new(move || settings().inline_budget),
    };
    let prepare = options.clone();
    let execute = options.clone();
    ToolDefinition {
        name: CODEMODE_TOOL_NAME.to_owned(),
        label: CODEMODE_TOOL_NAME.to_owned(),
        // Replaced with the declarations of the callable tools when the tool
        // is activated.
        description: create_codemode_description(&[], &CodemodeDescriptionOptions {
            models: true,
            ..Default::default()
        }),
        prompt_snippet: Some("Run JavaScript that calls other tools".to_owned()),
        prompt_guidelines: Some(vec![
            "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter large output, instead of many separate calls.".to_owned(),
        ]),
        parameters: codemode_schema(),
        // Capable models write the script as raw text instead of a JSON-escaped
        // string (tool.ts:380).
        constrained_sampling: Some(codemode_constrained_sampling()),
        output_schema: None,
        // Scripts must not start other scripts (tool.ts:377).
        exposure: ToolExposure::ModelOnly,
        namespace: None,
        annotations: None,
        default_active: Some(false),
        prepare_loadout: Some(Arc::new(move |loadout| {
            Ok(Some(prepare_codemode_loadout(loadout, &prepare)))
        })),
        render_shell: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |request: ToolExecuteRequest, ctx: rpi_ext_host::api::ExtensionContext| {
            let options = execute.clone();
            Box::pin(async move { execute_codemode(request, ctx, options).await })
        }),
        render_call: None,
        render_result: None,
    }
}

/// `constrainedSampling` (tool.ts:380): the Lark grammar that fixes the
/// optional `// @options:` line; the OpenAI adapters consume the grammar
/// variants when the model supports grammar tools.
pub fn codemode_constrained_sampling() -> Value {
    json!({
        "type": "grammar",
        "variants": { "openai_lark": rpi_codemode::CODEMODE_SOURCE_GRAMMAR },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_constrained_sampling_deserializes() {
        let parsed: rpi_ai::types::ConstrainedSampling =
            serde_json::from_value(codemode_constrained_sampling()).expect("grammar shape");
        match parsed {
            rpi_ai::types::ConstrainedSampling::Config(
                rpi_ai::types::ConstrainedSamplingConfig::Grammar { variants },
            ) => {
                assert_eq!(
                    variants.openai_lark.as_deref(),
                    Some(rpi_codemode::CODEMODE_SOURCE_GRAMMAR)
                );
            }
            other => panic!("expected grammar config, got {other:?}"),
        }
    }
}
