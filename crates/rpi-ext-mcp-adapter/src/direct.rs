//! directTools: register cached MCP tools as first-class rpi tools and keep
//! them in sync (added/updated/deactivated by fingerprint) (FR-P1-01, design
//! §3.8).
//!
//! Port of `direct-tools.ts` + the tool-surface half of `index.ts`
//! (`registerDirectTool` / `syncDirectTools` / `deactivateTools` /
//! `syncProxyTool` / `directToolFingerprint`) + `parseDirectToolSelectors`
//! (`metadata-cache.ts:122-146`) + `normalizeDirectToolInputSchema`
//! (`utils.ts:277-283`) @ pi-mcp-adapter v2.24.0 (3d953f90).
//!
//! Deactivation prefers the host `unregisterTool` method (TE01, ADR-0015);
//! names it cannot remove fall back to the upstream
//! getActiveTools/setActiveTools path with `fallbackDeactivatedTools`
//! tracking (design §3.8).
//!
//! `freezeDirectTools` is the prompt-cache red line (design R7): once frozen
//! (after the initial post-init sync), metadata-update-triggered syncs are
//! skipped; explicit `mcp({ connect })` still syncs (index.ts:822-824).
//!
//! P1-wave scope: OAuth auto-auth (FR-P1-04) and session recovery
//! (FR-P1-08) are wired into the direct executor (TE-D09/TE-D11); MCP UI
//! sessions (P2) are absent; the approveTools approval gate (FR-P1-07 /
//! R7.2.2) is wired in `execute_direct_tool` (TE21).

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use tracing::warn;

use crate::cache::{is_server_cache_valid, MetadataCache};
use crate::metadata::{
    format_tool_name, get_tool_name_candidates_with, has_tool_filters, is_tool_allowed,
    resolve_tool_prefix, resource_name_to_tool_name, McpConfig, ToolPrefix,
    ToolSelectorCandidateIndex,
};
use crate::utils::truncate_at_word;

/// `BUILTIN_NAMES` (direct-tools.ts:25): direct tools may not shadow these.
const BUILTIN_NAMES: [&str; 8] = ["read", "bash", "edit", "write", "grep", "find", "ls", "mcp"];
/// `DIRECT_TOOLS_ADVISORY_THRESHOLD` (direct-tools.ts:27).
pub const DIRECT_TOOLS_ADVISORY_THRESHOLD: usize = 75;

/// `DirectToolSpec` (types.ts:570-579, minus P2 UI fields).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirectToolSpec {
    pub server_name: String,
    pub original_name: String,
    pub prefixed_name: String,
    pub description: String,
    pub input_schema: Option<Value>,
    pub resource_uri: Option<String>,
}

/// `parseDirectToolSelectors` (metadata-cache.ts:122-146): `server` or
/// `server/tool` selectors; trailing slashes stripped.
pub fn parse_direct_tool_selectors(
    selectors: &[String],
) -> (HashSet<String>, HashMap<String, HashSet<String>>) {
    let mut servers = HashSet::new();
    let mut tools: HashMap<String, HashSet<String>> = HashMap::new();
    for selector in selectors {
        let selector = selector.trim_end_matches('/');
        if let Some((server, tool)) = selector.split_once('/') {
            let tool = tool.split('/').next().unwrap_or(tool);
            if !server.is_empty() && !tool.is_empty() {
                tools
                    .entry(server.to_string())
                    .or_default()
                    .insert(tool.to_string());
            } else if !server.is_empty() {
                servers.insert(server.to_string());
            }
        } else if !selector.is_empty() {
            servers.insert(selector.to_string());
        }
    }
    (servers, tools)
}

/// `createToolSelectorCandidateIndex` inside `resolveDirectTools`
/// (direct-tools.ts:211-228 @ 10a45367): current (non-legacy) candidates of
/// every configured server with a valid cache, so a legacy-only
/// include/exclude selector cannot sweep another tool's current name.
fn direct_selector_candidate_index(
    config: &McpConfig,
    cache: &MetadataCache,
    prefix: ToolPrefix,
) -> ToolSelectorCandidateIndex {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !candidates.contains(&value) {
            candidates.push(value);
        }
    };
    for (other_server_name, other_definition) in &config.mcp_servers {
        if other_definition.is_disabled() {
            continue;
        }
        let Some(other_cache) = cache.servers.get(other_server_name) else {
            continue;
        };
        if !is_server_cache_valid(
            other_cache,
            other_definition,
            crate::cache::CACHE_MAX_AGE_MS,
            now_ms(),
        ) {
            continue;
        }
        let other_prefix = resolve_tool_prefix(Some(other_definition), prefix);
        for tool in &other_cache.tools {
            for candidate in
                get_tool_name_candidates_with(&tool.name, other_server_name, other_prefix, false)
            {
                push(candidate);
            }
        }
        if other_definition.exposes_resources() {
            for resource in &other_cache.resources {
                let base_name = format!("read_{}", resource_name_to_tool_name(&resource.name));
                for candidate in get_tool_name_candidates_with(
                    &base_name,
                    other_server_name,
                    other_prefix,
                    false,
                ) {
                    push(candidate);
                }
            }
        }
    }
    ToolSelectorCandidateIndex::from_candidates(candidates)
}

/// `resolveDirectTools` (direct-tools.ts:114-208).
pub fn resolve_direct_tools(
    config: &McpConfig,
    cache: Option<&MetadataCache>,
    prefix: ToolPrefix,
    env_override: Option<&[String]>,
    unavailable_servers: &HashSet<String>,
) -> Vec<DirectToolSpec> {
    let mut specs = Vec::new();
    let Some(cache) = cache else { return specs };
    let mut seen_names: HashSet<String> = HashSet::new();

    let env_selection = env_override.map(parse_direct_tool_selectors);
    let global_direct = config
        .settings
        .as_ref()
        .and_then(|s| s.get("directTools"))
        .and_then(Value::as_bool);

    for (server_name, definition) in &config.mcp_servers {
        if definition.is_disabled() {
            continue;
        }
        let Some(server_cache) = cache.servers.get(server_name) else {
            continue;
        };
        if !is_server_cache_valid(
            server_cache,
            definition,
            crate::cache::CACHE_MAX_AGE_MS,
            now_ms(),
        ) {
            continue;
        }

        let tool_filter: ToolFilter = match &env_selection {
            Some((servers, tools)) => {
                if servers.contains(server_name) {
                    ToolFilter::All
                } else if let Some(set) = tools.get(server_name) {
                    ToolFilter::List(set.iter().cloned().collect())
                } else {
                    ToolFilter::None
                }
            }
            None => match definition.get("directTools") {
                Some(Value::Bool(true)) => ToolFilter::All,
                Some(Value::Array(list)) => ToolFilter::List(
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                ),
                Some(_) => ToolFilter::None,
                None => match global_direct {
                    Some(true) => ToolFilter::All,
                    _ => ToolFilter::None,
                },
            },
        };
        if matches!(tool_filter, ToolFilter::None) {
            continue;
        }

        let effective_prefix = resolve_tool_prefix(Some(definition), prefix);
        let selector_candidate_index = has_tool_filters(definition)
            .then(|| direct_selector_candidate_index(config, cache, prefix));

        for tool in &server_cache.tools {
            if !tool_filter.allows(&tool.name) {
                continue;
            }
            if !is_tool_allowed(
                &tool.name,
                server_name,
                effective_prefix,
                definition.include_tools(),
                definition.exclude_tools(),
                selector_candidate_index.as_ref(),
            ) {
                continue;
            }
            let prefixed_name = format_tool_name(&tool.name, server_name, effective_prefix);
            if BUILTIN_NAMES.contains(&prefixed_name.as_str()) {
                warn!(tool = %prefixed_name, "MCP: skipping direct tool (collides with builtin)");
                continue;
            }
            if !seen_names.insert(prefixed_name.clone()) {
                warn!(tool = %prefixed_name, server = %server_name, "MCP: skipping duplicate direct tool");
                continue;
            }
            specs.push(DirectToolSpec {
                server_name: server_name.clone(),
                original_name: tool.name.clone(),
                prefixed_name,
                description: tool.description.clone().unwrap_or_default(),
                input_schema: tool.input_schema.clone(),
                resource_uri: None,
            });
        }

        if definition.exposes_resources() {
            for resource in &server_cache.resources {
                let base_name = format!("read_{}", resource_name_to_tool_name(&resource.name));
                if !tool_filter.allows(&base_name) {
                    continue;
                }
                if !is_tool_allowed(
                    &base_name,
                    server_name,
                    effective_prefix,
                    definition.include_tools(),
                    definition.exclude_tools(),
                    selector_candidate_index.as_ref(),
                ) {
                    continue;
                }
                let prefixed_name = format_tool_name(&base_name, server_name, effective_prefix);
                if BUILTIN_NAMES.contains(&prefixed_name.as_str()) {
                    warn!(tool = %prefixed_name, "MCP: skipping direct resource tool (collides with builtin)");
                    continue;
                }
                if !seen_names.insert(prefixed_name.clone()) {
                    warn!(tool = %prefixed_name, server = %server_name, "MCP: skipping duplicate direct resource tool");
                    continue;
                }
                specs.push(DirectToolSpec {
                    server_name: server_name.clone(),
                    original_name: base_name,
                    prefixed_name,
                    description: resource
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("Read resource: {}", resource.uri)),
                    input_schema: None,
                    resource_uri: Some(resource.uri.clone()),
                });
            }
        }
    }

    // direct-tools.ts:284-292 @ 10a45367 (#434): the emitted set drops
    // servers in active failure backoff; the advisory threshold counts the
    // emitted set.
    let emitted: Vec<DirectToolSpec> = if unavailable_servers.is_empty() {
        specs
    } else {
        specs
            .into_iter()
            .filter(|spec| !unavailable_servers.contains(&spec.server_name))
            .collect()
    };

    // #358/#412 (direct-tools.ts:288-290 @ 10a45367): the advisory is
    // gated by `warnOnLargeDirectTools !== false` and its text explains
    // how to hide it.
    let advisory_enabled = config
        .settings
        .as_ref()
        .and_then(|s| s.get("warnOnLargeDirectTools"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if advisory_enabled && emitted.len() >= DIRECT_TOOLS_ADVISORY_THRESHOLD {
        warn!(
            count = emitted.len(),
            "MCP: {} direct tools resolved. Each direct tool adds prompt context; README guidance recommends targeted sets of 5-20 tools and using the proxy or an explicit string[] when 75+ direct tools would be registered. Set settings.warnOnLargeDirectTools to false to hide this advisory.",
            emitted.len()
        );
    }
    emitted
}

enum ToolFilter {
    All,
    List(Vec<String>),
    None,
}

impl ToolFilter {
    fn allows(&self, name: &str) -> bool {
        match self {
            ToolFilter::All => true,
            ToolFilter::List(list) => list.iter().any(|n| n == name),
            ToolFilter::None => false,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `directToolFingerprint` (index.ts:140-151): JSON of the spec subset, keys
/// in upstream insertion order, absent fields omitted (JSON.stringify drops
/// undefined).
pub fn direct_tool_fingerprint(spec: &DirectToolSpec) -> String {
    let mut map = serde_json::Map::new();
    map.insert("serverName".to_string(), json!(spec.server_name));
    map.insert("originalName".to_string(), json!(spec.original_name));
    map.insert("prefixedName".to_string(), json!(spec.prefixed_name));
    map.insert("description".to_string(), json!(spec.description));
    if let Some(schema) = &spec.input_schema {
        map.insert("inputSchema".to_string(), schema.clone());
    }
    if let Some(uri) = &spec.resource_uri {
        map.insert("resourceUri".to_string(), json!(uri));
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_default()
}

/// `normalizeDirectToolInputSchema` (utils.ts:277-283): strip `$schema` /
/// `additionalProperties`; non-object schemas become an empty object schema.
pub fn normalize_direct_tool_input_schema(schema: Option<&Value>) -> Value {
    let mut normalized = match schema.and_then(Value::as_object) {
        Some(map) => map.clone(),
        None => {
            return json!({ "type": "object", "properties": {} });
        }
    };
    normalized.shift_remove("$schema");
    normalized.shift_remove("additionalProperties");
    Value::Object(normalized)
}

/// The registerTool definition for a direct tool (index.ts:153-163).
pub fn direct_tool_definition(spec: &DirectToolSpec) -> Value {
    let snippet = truncate_at_word(&spec.description, 100);
    let prompt_snippet = if snippet.is_empty() {
        format!("MCP tool from {}", spec.server_name)
    } else {
        snippet
    };
    json!({
        "definition": {
            "name": spec.prefixed_name,
            "label": format!("MCP: {}", spec.original_name),
            "description": if spec.description.is_empty() { "(no description)" } else { &spec.description },
            "promptSnippet": prompt_snippet,
            "parameters": normalize_direct_tool_input_schema(spec.input_schema.as_ref()),
            // createMcpDirectToolCallRenderer + renderMcpToolResult for direct
            // tools (index.ts:161-162): the call lines show displayName +
            // jsonish args, the shared MCP result renderer handles collapse.
            // Both render through the host's {"kind":"render"} dispatch;
            // toolCall uses the registered (prefixed) name as displayName.
            "renderCall": true,
            "renderResult": true,
        },
    })
}

/// The host-surface operations the sync logic needs (implemented over host
/// calls in `lib.rs`; faked in tests).
pub trait ToolSurface {
    fn register_tool(&mut self, definition: Value);
    /// Host `unregisterTool` (TE01): true when the registry entry was
    /// removed.
    fn unregister_tool(&mut self, name: &str) -> bool;
    /// `None` when active tools are unavailable during extension loading
    /// (index.ts:172-180 `getActiveToolsIfReady`).
    fn get_active_tools(&mut self) -> Option<Vec<String>>;
    fn set_active_tools(&mut self, names: Vec<String>);
}

/// `syncDirectTools` diff outcome (index.ts:201-237).
#[derive(Debug, Default, PartialEq)]
pub struct SyncReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub deactivated: Vec<String>,
}

/// Registered-surface state (`registeredDirectTools` +
/// `fallbackDeactivatedTools` in index.ts).
#[derive(Default)]
pub struct DirectToolRegistry {
    pub registered: HashMap<String, String>,
    pub fallback_deactivated: HashSet<String>,
    pub specs: HashMap<String, DirectToolSpec>,
}

impl DirectToolRegistry {
    /// `syncDirectTools` (index.ts:201-237) + `deactivateTools`
    /// (index.ts:182-199).
    pub fn sync(&mut self, specs: &[DirectToolSpec], surface: &mut dyn ToolSurface) -> SyncReport {
        let next_names: HashSet<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        let mut report = SyncReport::default();

        for spec in specs {
            let fingerprint = direct_tool_fingerprint(spec);
            let previous = self.registered.get(spec.prefixed_name.as_str());
            if previous == Some(&fingerprint) {
                continue;
            }
            let was_registered = previous.is_some();
            surface.register_tool(direct_tool_definition(spec));
            self.registered
                .insert(spec.prefixed_name.clone(), fingerprint);
            self.specs.insert(spec.prefixed_name.clone(), spec.clone());
            if self.fallback_deactivated.remove(&spec.prefixed_name) {
                if let Some(mut active) = surface.get_active_tools() {
                    if !active.contains(&spec.prefixed_name) {
                        active.push(spec.prefixed_name.clone());
                        surface.set_active_tools(active);
                    }
                }
            }
            if was_registered {
                report.updated.push(spec.prefixed_name.clone());
            } else {
                report.added.push(spec.prefixed_name.clone());
            }
        }

        let registered_names: Vec<String> = self.registered.keys().cloned().collect();
        for name in registered_names {
            if next_names.contains(name.as_str()) {
                continue;
            }
            self.registered.remove(&name);
            self.specs.remove(&name);
            report.deactivated.push(name);
        }
        self.deactivate(&report.deactivated.clone(), surface);
        report
    }

    /// `deactivateTools` (index.ts:182-199): host `unregisterTool` first;
    /// leftovers go through the active-tools fallback and are tracked for
    /// re-activation on re-registration.
    fn deactivate(&mut self, tool_names: &[String], surface: &mut dyn ToolSurface) {
        if tool_names.is_empty() {
            return;
        }
        let mut unregistered: Vec<&str> = Vec::new();
        let mut fallback: Vec<&str> = Vec::new();
        for name in tool_names {
            if surface.unregister_tool(name) {
                unregistered.push(name);
            } else {
                fallback.push(name);
            }
        }
        let remove: HashSet<&str> = tool_names.iter().map(String::as_str).collect();
        match surface.get_active_tools() {
            None => {
                for name in fallback {
                    self.fallback_deactivated.insert(name.to_string());
                }
            }
            Some(active) if active.is_empty() => {
                for name in fallback {
                    self.fallback_deactivated.insert(name.to_string());
                }
            }
            Some(active) => {
                let next: Vec<String> = active
                    .iter()
                    .filter(|name| !remove.contains(name.as_str()))
                    .cloned()
                    .collect();
                if next.len() != active.len() {
                    for name in fallback {
                        self.fallback_deactivated.insert(name.to_string());
                    }
                    surface.set_active_tools(next);
                }
            }
        }
    }

    pub fn spec(&self, prefixed_name: &str) -> Option<&DirectToolSpec> {
        self.specs.get(prefixed_name)
    }
}

/// `shouldRegisterProxyTool` truth table (index.ts:851-855).
pub fn should_register_proxy_tool(
    config: &McpConfig,
    direct_specs: &[DirectToolSpec],
    missing_direct_servers: &[String],
) -> bool {
    let disable_proxy = config
        .settings
        .as_ref()
        .and_then(|s| s.get("disableProxyTool"))
        .and_then(Value::as_bool)
        == Some(true);
    !disable_proxy || direct_specs.is_empty() || !missing_direct_servers.is_empty()
}

/// `buildProxyDescription` (direct-tools.ts:302-330 @ 10a45367, #432):
/// pure function of config. Live counts/instructions/connection state are
/// deliberately absent so re-registering the proxy tool never rewrites the
/// cached prompt prefix; `mcp({ })` carries the runtime counts.
pub fn build_proxy_description(config: &McpConfig) -> String {
    let mut desc = "MCP gateway — server status, tool search/describe, auth, and single MCP tool calls. When one request needs several MCP calls with logic between them, use mcpScript. Non-MCP Pi tools should be called directly, not through mcp.\n".to_string();

    let server_names: Vec<&str> = config
        .mcp_servers
        .iter()
        .filter(|(_, definition)| !definition.is_disabled())
        .map(|(name, _)| name.as_str())
        .collect();
    if !server_names.is_empty() {
        desc.push_str(&format!("\nServers: {}\n", server_names.join(", ")));
    }

    let disabled: Vec<&str> = config
        .mcp_servers
        .iter()
        .filter(|(_, definition)| definition.is_disabled())
        .map(|(name, _)| name.as_str())
        .collect();
    if !disabled.is_empty() {
        desc.push_str(&format!(
            "\nDisabled servers (enable with /mcp enable <server> and /reload): {}\n",
            disabled.join(", ")
        ));
    }

    desc.push_str("\nUsage:\n");
    desc.push_str("  mcp({ })                              → Show server status and tool counts\n");
    desc.push_str("  mcp({ server: \"name\" })               → List tools from server\n");
    desc.push_str(
        "  mcp({ search: \"query\" })              → Search MCP tools by name/description\n",
    );
    desc.push_str("  mcp({ describe: \"tool_name\" })        → Show tool details and parameters\n");
    desc.push_str(
        "  mcp({ instructions: \"name\" })         → Show full server usage instructions\n",
    );
    desc.push_str(
        "  mcp({ connect: \"server-name\" })       → Connect to a server and refresh metadata\n",
    );
    desc.push_str("  mcp({ tool: \"name\", args: { key: \"value\" } })         → Call a tool (object args; JSON string also accepted)\n");
    desc.push_str("  mcp({ action: \"ui-messages\" })        → Retrieve accumulated messages from completed UI sessions\n");
    desc.push_str("  mcp({ action: \"auth-start\", server: \"name\" })      → Start manual OAuth and get a browser URL\n");
    desc.push_str("  mcp({ action: \"auth-complete\", server: \"name\", args: { redirectUrl: \"...\" } }) → Complete manual OAuth\n");
    desc.push_str("\nMode: action > tool (call) > connect > describe > instructions > search > server (list) > nothing (status)");
    desc
}

/// The direct tool executor (direct-tools.ts:300-557 `createDirectToolExecutor`),
/// with the approveTools approval gate wired (FR-P1-07 / R7.2.2, TE21).
/// MCP UI sessions are P2 and absent. Auto-auth (FR-P1-04) and session
/// recovery (FR-P1-08) are wired in (TE-D09/TE-D11).
pub async fn execute_direct_tool(
    runtime: &crate::proxy::McpRuntime,
    spec: &DirectToolSpec,
    params: &Value,
) -> Value {
    let config = &runtime.config;

    // #430 (direct-tools.ts:40-89 @ 10a45367,
    // `settings.strictDirectToolArguments: true`): recover one model-
    // emitted JSON layer for schema-declared object/array properties, then
    // validate the complete input against the advertised schema. Upstream
    // runs this in `prepareArguments` (host-side, throws TypeError); the
    // ABI has no prepare channel (candidate gap, extension-abi.md §8.5),
    // so the equivalent boundary is the executor entry — same failure
    // message, surfaced as an error result (details.error =
    // `invalid_args`, re-flagged isError by the tool_result hook).
    let strict_args = config
        .settings
        .as_ref()
        .and_then(|s| s.get("strictDirectToolArguments"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if strict_args {
        match prepare_direct_tool_arguments(&spec.input_schema, params) {
            Ok(prepared) => {
                return execute_direct_tool_inner(runtime, spec, &prepared, config).await;
            }
            Err(message) => {
                return json!({
                    "content": [{ "type": "text", "text": message }],
                    "details": {
                        "error": "invalid_args",
                        "server": spec.server_name,
                        "message": message,
                    },
                });
            }
        }
    }
    execute_direct_tool_inner(runtime, spec, params, config).await
}

/// `prepareDirectToolArguments` (direct-tools.ts:40-89 @ 10a45367):
/// one-layer JSON recovery + strict schema validation.
fn prepare_direct_tool_arguments(
    input_schema: &Option<Value>,
    args: &Value,
) -> Result<Value, String> {
    let Some(schema) = input_schema else {
        return Ok(args.clone());
    };
    let Some(object_schema) = schema.as_object() else {
        return Ok(args.clone());
    };
    if object_schema.get("type").and_then(Value::as_str) != Some("object") {
        return Ok(args.clone());
    }

    // One-layer recovery: string values for schema-declared object/array
    // properties that parse to the declared shape are replaced.
    let mut prepared = args.clone();
    if let (Some(input), Some(properties)) = (args.as_object(), object_schema.get("properties")) {
        if let Some(properties) = properties.as_object() {
            for (name, property_schema) in properties {
                let Some(current) = input.get(name) else {
                    continue;
                };
                let Some(text) = current.as_str() else {
                    continue;
                };
                let Some(property_schema) = property_schema.as_object() else {
                    continue;
                };
                let expected_type = property_schema.get("type").and_then(Value::as_str);
                if expected_type != Some("object") && expected_type != Some("array") {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    let matches = match expected_type {
                        Some("array") => parsed.is_array(),
                        Some("object") => parsed.is_object(),
                        _ => false,
                    };
                    if matches {
                        if let Some(map) = prepared.as_object_mut() {
                            map.insert(name.clone(), parsed);
                        }
                    }
                }
                // Parse failures and shape mismatches fall through to the
                // validation below (upstream comment: "Validation below
                // reports malformed or shape-incompatible values").
            }
        }
    }

    // Strict validation (upstream TypeBox `Check`/`Errors`; the jsonschema
    // crate is the workspace validator — same draft family).
    let validator = match jsonschema::validator_for(schema) {
        Ok(validator) => validator,
        Err(_) => return Ok(prepared), // unvalidatable schema: pass through
    };
    if validator.is_valid(&prepared) {
        return Ok(prepared);
    }
    let errors: Vec<(String, String, String)> = validator
        .iter_errors(&prepared)
        .map(|error| {
            // TypeBox reports `instancePath`/`keyword`/`message`; the
            // jsonschema crate exposes the instance location and the
            // schema location (whose last segment is the failing keyword),
            // so the envelope carries real values rather than constants.
            let instance = error.instance_path.as_str();
            let instance_path = if instance.is_empty() {
                "/".to_string()
            } else {
                instance.to_string()
            };
            let keyword = error
                .schema_path
                .as_str()
                .rsplit('/')
                .find(|segment| !segment.is_empty())
                .unwrap_or("")
                .to_string();
            (instance_path, keyword, error.to_string())
        })
        .collect();
    let total = errors.len();
    let issues: Vec<Value> = errors
        .iter()
        .take(8)
        .map(|(instance_path, keyword, message)| {
            json!({
                "instancePath": instance_path,
                "keyword": keyword,
                "message": message,
            })
        })
        .collect();
    let payload = json!({
        "issues": issues,
        "total": total,
        "truncated": total > 8,
    });
    Err(format!(
        "MCP direct tool arguments do not match the advertised input schema: {payload}"
    ))
}

async fn execute_direct_tool_inner(
    runtime: &crate::proxy::McpRuntime,
    spec: &DirectToolSpec,
    params: &Value,
    config: &crate::metadata::McpConfig,
) -> Value {
    // The non-strict path's `config` binding is the same runtime config;
    // re-bound here so the inner body keeps one variable.
    let Some(definition) = config.mcp_servers.get(&spec.server_name) else {
        let message = format!("MCP server \"{}\" not connected", spec.server_name);
        return json!({
            "content": [{ "type": "text", "text": message }],
            "details": { "error": "not_connected", "server": spec.server_name },
        });
    };
    if definition.is_disabled() {
        let message = format!(
            "MCP server \"{}\" is disabled. Run /mcp enable {} and /reload to enable it.",
            spec.server_name, spec.server_name
        );
        return json!({
            "content": [{ "type": "text", "text": message }],
            "details": { "error": "server_disabled", "server": spec.server_name, "message": message },
        });
    }

    // TE-D09 (direct-tools.ts:338-369): needs-auth after lazy connect →
    // one auto-auth attempt → lazy connect again.
    let mut auto_auth_attempted = false;
    let mut connected = crate::proxy::lazy_connect(runtime, &spec.server_name).await;
    if !connected
        && runtime
            .manager
            .get_connection(&spec.server_name)
            .is_some_and(|c| c.status() == crate::manager::ConnectionStatus::NeedsAuth)
    {
        auto_auth_attempted = true;
        match crate::proxy::attempt_auto_auth(runtime, &spec.server_name).await {
            Err(message) => {
                return json!({
                    "content": [{ "type": "text", "text": message }],
                    "details": { "error": "auth_required", "server": spec.server_name, "message": message },
                });
            }
            Ok(true) => {
                connected = crate::proxy::lazy_connect(runtime, &spec.server_name).await;
            }
            Ok(false) => {}
        }
    }

    if !connected {
        if runtime
            .manager
            .get_connection(&spec.server_name)
            .is_some_and(|c| c.status() == crate::manager::ConnectionStatus::NeedsAuth)
        {
            let message = crate::proxy::auth_required_message(config, &spec.server_name);
            return json!({
                "content": [{ "type": "text", "text": message }],
                "details": {
                    "error": "auth_required", "server": spec.server_name,
                    "message": message, "autoAuthAttempted": auto_auth_attempted,
                },
            });
        }
        let failed_ago = runtime.failures.failure_age_seconds(&spec.server_name);
        let message = match failed_ago {
            Some(ago) => format!(
                "MCP server \"{}\" not available (failed {ago}s ago)",
                spec.server_name
            ),
            None => format!("MCP server \"{}\" not available", spec.server_name),
        };
        return json!({
            "content": [{ "type": "text", "text": message }],
            "details": { "error": "server_unavailable", "server": spec.server_name },
        });
    }

    let connection = runtime.manager.get_connection(&spec.server_name);
    let Some(connection) = connection else {
        let message = format!("MCP server \"{}\" not connected", spec.server_name);
        return json!({
            "content": [{ "type": "text", "text": message }],
            "details": { "error": "not_connected", "server": spec.server_name },
        });
    };
    if connection.status() != crate::manager::ConnectionStatus::Connected {
        let message = format!("MCP server \"{}\" not connected", spec.server_name);
        return json!({
            "content": [{ "type": "text", "text": message }],
            "details": { "error": "not_connected", "server": spec.server_name },
        });
    }

    // R7.2.2.1–.4 / FR-P1-07: approval gate (direct-tools.ts:423-445 @
    // 10a45367). The definition/argument identity and the session grant
    // persistence live in `crate::approval`.
    let tool_meta = crate::metadata::ToolMetadata {
        name: spec.prefixed_name.clone(),
        original_name: spec.original_name.clone(),
        description: spec.description.clone(),
        resource_uri: spec.resource_uri.clone(),
        input_schema: spec.input_schema.clone(),
    };
    let approval_origin = if spec.resource_uri.is_some() {
        crate::approval::ApprovalOrigin::Resource
    } else {
        crate::approval::ApprovalOrigin::Direct
    };
    let approval_ui = runtime
        .approval_ui
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let approval_result = crate::approval::ensure_tool_call_approved(
        config,
        &runtime.approval,
        &spec.server_name,
        &tool_meta,
        params,
        approval_origin,
        None,
        approval_ui.as_deref(),
        || {
            let metadata = runtime
                .tool_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            crate::approval::approval_candidate_context(
                config,
                &metadata,
                &spec.server_name,
                &spec.original_name,
            )
        },
    );
    if approval_result != crate::approval::ToolCallApprovalResult::Ok {
        let (content, details) = crate::approval::approval_rejection_details(
            &approval_result,
            &spec.server_name,
            &spec.original_name,
            None,
        );
        return json!({ "content": content, "details": details });
    }

    let guard_options = crate::guard::resolve_guard_options(config.settings.as_ref());
    // #430 (direct-tools.ts:501/551/566/583 @ 10a45367,
    // `settings.directToolResultDetails: "bounded"`): keep the raw MCP
    // result in `details.mcpResult` (bounded by the guard's 16 KiB cap).
    let bounded_details = config
        .settings
        .as_ref()
        .and_then(|s| s.get("directToolResultDetails"))
        .and_then(Value::as_str)
        == Some("bounded");
    let request_timeout = runtime.manager.request_timeout(definition);
    runtime.manager.touch(&spec.server_name);
    runtime.manager.increment_in_flight(&spec.server_name);

    // TE-D11: session recovery — if the tool call fails with a stale
    // session (HTTP 404 with session id), reconnect once and retry.
    let connection_for_recovery = connection.clone();
    let result: Result<(Value, bool), String> = async {
        let Some(client) = connection.client.clone() else {
            return Err("server not connected".to_string());
        };
        let had_session_id = client.session_id().is_some();
        let call_result = crate::proxy::run_tool_call(
            &client,
            spec.resource_uri.as_deref(),
            &spec.original_name,
            params.clone(),
            request_timeout,
        )
        .await;

        match call_result {
            Ok(ok) => Ok(ok),
            Err(error) => {
                if crate::session_recovery::is_terminated_session(&error, had_session_id) {
                    tracing::debug!(server = %spec.server_name, "MCP direct session expired; reconnecting");
                    match runtime
                        .manager
                        .reconnect(&spec.server_name, definition, &connection_for_recovery)
                        .await
                    {
                        Ok(fresh)
                            if fresh.status() == crate::manager::ConnectionStatus::Connected =>
                        {
                            let Some(fresh_client) = &fresh.client else {
                                return Err(error.to_string());
                            };
                            crate::proxy::run_tool_call(
                                fresh_client,
                                spec.resource_uri.as_deref(),
                                &spec.original_name,
                                params.clone(),
                                request_timeout,
                            )
                            .await
                            .map_err(|e| e.to_string())
                        }
                        // TE-D09 (direct-tools.ts:414-431): the fresh
                        // session needs auth — one auto-auth attempt, then
                        // retry once on the re-authed connection; otherwise
                        // throw SessionRecoveryAuthRequiredError, whose
                        // message renders as the error text below.
                        Ok(fresh)
                            if fresh.status()
                                == crate::manager::ConnectionStatus::NeedsAuth =>
                        {
                            let message =
                                match crate::proxy::attempt_auto_auth(runtime, &spec.server_name)
                                    .await
                                {
                                    Err(message) => message,
                                    Ok(true) => {
                                        let reauthed =
                                            runtime.manager.get_connection(&spec.server_name);
                                        match reauthed.map(|c| (c.status(), c.client.clone())) {
                                            Some((
                                                crate::manager::ConnectionStatus::Connected,
                                                Some(client),
                                            )) => {
                                                return crate::proxy::run_tool_call(
                                                    &client,
                                                    spec.resource_uri.as_deref(),
                                                    &spec.original_name,
                                                    params.clone(),
                                                    request_timeout,
                                                )
                                                .await
                                                .map_err(|e| e.to_string());
                                            }
                                            _ => crate::session_recovery::auth_required_error_message(
                                                &spec.server_name,
                                            ),
                                        }
                                    }
                                    Ok(false) => crate::session_recovery::auth_required_error_message(
                                        &spec.server_name,
                                    ),
                                };
                            Err(message)
                        }
                        _ => Err(error.to_string()),
                    }
                } else {
                    Err(error.to_string())
                }
            }
        }
    }
    .await;
    runtime.manager.decrement_in_flight(&spec.server_name);
    runtime.manager.touch(&spec.server_name);

    match result {
        Ok((value, is_resource)) => {
            if is_resource {
                let content = value
                    .get("contents")
                    .and_then(Value::as_array)
                    .map(|a| crate::proxy::transform_mcp_resource_contents(a))
                    .unwrap_or_default();
                let content = if content.is_empty() {
                    vec![json!({ "type": "text", "text": "(empty resource)" })]
                } else {
                    content
                };
                let guarded = crate::guard::guard_mcp_output(
                    content,
                    &crate::guard::GuardOptions {
                        raw_mcp_result: bounded_details.then(|| value.clone()),
                        ..guard_options.clone()
                    },
                );
                let mut details = json!({
                    "server": spec.server_name,
                    "resourceUri": spec.resource_uri,
                });
                merge(&mut details, &crate::guard::guarded_mcp_details(&guarded));
                return json!({ "content": guarded.content, "details": details });
            }
            if value.get("isError").and_then(Value::as_bool) == Some(true) {
                let content = value
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| crate::proxy::transform_mcp_content(a))
                    .unwrap_or_default();
                let content = if content.is_empty() {
                    vec![json!({ "type": "text", "text": "(empty result)" })]
                } else {
                    content
                };
                let schema_text = spec
                    .input_schema
                    .as_ref()
                    .map(|s| {
                        format!(
                            "\n\nExpected parameters:\n{}",
                            crate::metadata::format_schema(s, "  ")
                        )
                    })
                    .unwrap_or_default();
                let guarded = crate::guard::guard_mcp_output(
                    content,
                    &crate::guard::GuardOptions {
                        prefix: Some("Error: ".to_string()),
                        suffix: if schema_text.is_empty() {
                            None
                        } else {
                            Some(schema_text)
                        },
                        empty_text_fallback: Some("Tool execution failed".to_string()),
                        ..guard_options.clone()
                    },
                );
                let mut details = json!({
                    "error": "tool_error",
                    "server": spec.server_name,
                });
                merge(&mut details, &crate::guard::guarded_mcp_details(&guarded));
                return json!({ "content": guarded.content, "details": details });
            }
            let content = crate::proxy::resolve_mcp_result_content(&value);
            let content = if content.is_empty() {
                vec![json!({ "type": "text", "text": "(empty result)" })]
            } else {
                content
            };
            let guarded = crate::guard::guard_mcp_output(
                content,
                &crate::guard::GuardOptions {
                    raw_mcp_result: bounded_details.then(|| value.clone()),
                    ..guard_options.clone()
                },
            );
            let mut details = json!({
                "server": spec.server_name,
                "tool": spec.original_name,
            });
            merge(&mut details, &crate::guard::guarded_mcp_details(&guarded));
            json!({ "content": guarded.content, "details": details })
        }
        Err(message) => {
            let schema_text = spec
                .input_schema
                .as_ref()
                .map(|s| {
                    format!(
                        "\n\nExpected parameters:\n{}",
                        crate::metadata::format_schema(s, "  ")
                    )
                })
                .unwrap_or_default();
            let guarded = crate::guard::guard_mcp_output(
                vec![json!({ "type": "text", "text": message })],
                &crate::guard::GuardOptions {
                    prefix: Some("Failed to call tool: ".to_string()),
                    suffix: if schema_text.is_empty() {
                        None
                    } else {
                        Some(schema_text)
                    },
                    ..guard_options.clone()
                },
            );
            let mut details = json!({
                "error": "call_failed",
                "server": spec.server_name,
            });
            merge(&mut details, &crate::guard::guarded_mcp_details(&guarded));
            json!({ "content": guarded.content, "details": details })
        }
    }
}

fn merge(target: &mut Value, source: &Value) {
    if let (Some(t), Some(s)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in s {
            t.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CachedResource, CachedTool, ServerCacheEntry};
    use crate::metadata::ServerEntry;

    fn spec(name: &str, server: &str) -> DirectToolSpec {
        DirectToolSpec {
            server_name: server.to_string(),
            original_name: name.to_string(),
            prefixed_name: format!("{server}_{name}"),
            description: format!("{name} description"),
            input_schema: None,
            resource_uri: None,
        }
    }

    #[derive(Default)]
    struct FakeSurface {
        tools: HashMap<String, Value>,
        active: Vec<String>,
        can_unregister: bool,
    }

    impl ToolSurface for FakeSurface {
        fn register_tool(&mut self, definition: Value) {
            let name = definition["definition"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            self.tools.insert(name.clone(), definition);
            if !self.active.contains(&name) {
                self.active.push(name);
            }
        }
        fn unregister_tool(&mut self, name: &str) -> bool {
            if !self.can_unregister {
                return false;
            }
            self.active.retain(|n| n != name);
            self.tools.remove(name).is_some()
        }
        fn get_active_tools(&mut self) -> Option<Vec<String>> {
            Some(self.active.clone())
        }
        fn set_active_tools(&mut self, names: Vec<String>) {
            self.active = names;
        }
    }

    #[test]
    fn fingerprint_diff_drives_added_updated_deactivated() {
        let mut registry = DirectToolRegistry::default();
        let mut surface = FakeSurface {
            can_unregister: true,
            ..Default::default()
        };

        let report = registry.sync(&[spec("a", "s"), spec("b", "s")], &mut surface);
        assert_eq!(report.added, ["s_a", "s_b"]);
        assert!(report.updated.is_empty() && report.deactivated.is_empty());

        // unchanged fingerprint → no-op
        let report = registry.sync(&[spec("a", "s"), spec("b", "s")], &mut surface);
        assert_eq!(report, SyncReport::default());

        // updated description → updated
        let mut changed = spec("a", "s");
        changed.description = "new description".to_string();
        let report = registry.sync(&[changed, spec("b", "s")], &mut surface);
        assert_eq!(report.updated, ["s_a"]);

        // removed from specs → deactivated via unregisterTool
        let report = registry.sync(&[spec("a", "s")], &mut surface);
        assert_eq!(report.deactivated, ["s_b"]);
        assert!(!surface.tools.contains_key("s_b"));
    }

    #[test]
    fn fallback_deactivation_when_unregister_unavailable() {
        let mut registry = DirectToolRegistry::default();
        let mut surface = FakeSurface {
            can_unregister: false,
            ..Default::default()
        };
        registry.sync(&[spec("a", "s"), spec("b", "s")], &mut surface);
        let report = registry.sync(&[spec("a", "s")], &mut surface);
        assert_eq!(report.deactivated, ["s_b"]);
        // Fallback path: removed from active tools, tracked for re-activation.
        assert!(!surface.active.contains(&"s_b".to_string()));
        assert!(registry.fallback_deactivated.contains("s_b"));

        // Re-registration restores the active slot.
        registry.sync(&[spec("a", "s"), spec("b", "s")], &mut surface);
        assert!(surface.active.contains(&"s_b".to_string()));
        assert!(!registry.fallback_deactivated.contains("s_b"));
    }

    #[test]
    fn proxy_tool_truth_table() {
        let mut config = McpConfig::default();
        // disableProxyTool unset → always register
        assert!(should_register_proxy_tool(&config, &[], &[]));
        // disableProxyTool + no direct specs → register
        config.settings = Some(
            json!({ "disableProxyTool": true })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        assert!(should_register_proxy_tool(&config, &[], &[]));
        // disableProxyTool + specs + no missing → do NOT register
        assert!(!should_register_proxy_tool(&config, &[spec("a", "s")], &[]));
        // disableProxyTool + specs + missing cache → register
        assert!(should_register_proxy_tool(
            &config,
            &[spec("a", "s")],
            &["s".to_string()]
        ));
    }

    #[test]
    fn selector_parsing() {
        let (servers, tools) = parse_direct_tool_selectors(&[
            "github".to_string(),
            "xcodebuild/list_sims".to_string(),
            "trailing/".to_string(),
            "/".to_string(),
        ]);
        assert!(servers.contains("github"));
        assert!(servers.contains("trailing"));
        assert!(tools["xcodebuild"].contains("list_sims"));
    }

    #[test]
    fn resolve_direct_tools_from_cache() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({ "command": "node", "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        let definition = &config.mcp_servers["demo"];
        let hash = crate::cache::compute_server_hash(definition).expect("hash");
        cache.servers.insert(
            "demo".to_string(),
            ServerCacheEntry {
                config_hash: hash,
                tools: vec![CachedTool {
                    name: "search".to_string(),
                    description: Some("Search things".to_string()),
                    input_schema: None,
                    ..Default::default()
                }],
                resources: vec![CachedResource {
                    uri: "mcp://demo/doc".to_string(),
                    name: "Doc".to_string(),
                    description: None,
                }],
                cached_at: now_ms(),
                ..Default::default()
            },
        );
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["demo_search", "demo_read_doc"]);
        assert_eq!(specs[1].resource_uri.as_deref(), Some("mcp://demo/doc"));

        // per-server list filter
        config.mcp_servers.get_mut("demo").map(|d| {
            d.as_map_mut()
                .insert("directTools".to_string(), json!(["search"]))
        });
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        assert_eq!(specs.len(), 1);
    }

    /// #346 (direct-tools.ts:211-228 @ 10a45367): the cross-server selector
    /// candidate index suppresses a legacy-only include selector that would
    /// sweep another server's current name.
    #[test]
    fn direct_selector_index_suppresses_legacy_collisions() {
        let filtered = ServerEntry(
            json!({ "command": "node", "directTools": true, "includeTools": ["my_server_do_thing"] })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let other = ServerEntry(
            json!({ "command": "node", "directTools": true })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let mut config = McpConfig::default();
        config
            .mcp_servers
            .insert("my-server".to_string(), filtered.clone());
        config
            .mcp_servers
            .insert("my_server".to_string(), other.clone());
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        for (name, definition, tool_name) in [
            ("my-server", &filtered, "do_thing"),
            ("my_server", &other, "do_thing"),
        ] {
            cache.servers.insert(
                name.to_string(),
                ServerCacheEntry {
                    config_hash: crate::cache::compute_server_hash(definition).expect("hash"),
                    tools: vec![CachedTool {
                        name: tool_name.to_string(),
                        ..Default::default()
                    }],
                    cached_at: now_ms(),
                    ..Default::default()
                },
            );
        }

        // `my_server` owns the current name `my_server_do_thing` → the legacy
        // include selector on `my-server` is suppressed.
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["my_server_do_thing"]);

        // Rename the other server's tool → no collision → legacy selector applies.
        cache.servers.insert(
            "my_server".to_string(),
            ServerCacheEntry {
                config_hash: crate::cache::compute_server_hash(&other).expect("hash"),
                tools: vec![CachedTool {
                    name: "do_other".to_string(),
                    ..Default::default()
                }],
                cached_at: now_ms(),
                ..Default::default()
            },
        );
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["my-server_do_thing", "my_server_do_other"]);
    }

    /// #434：退避中的 server 不出现在 direct 面（#A4）。
    #[test]
    fn unavailable_servers_are_filtered_from_direct_tools() {
        let mut config = McpConfig::default();
        for name in ["alpha", "beta"] {
            config.mcp_servers.insert(
                name.to_string(),
                ServerEntry(
                    json!({ "command": "node", "directTools": true })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                ),
            );
        }
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        for name in ["alpha", "beta"] {
            let hash = crate::cache::compute_server_hash(&config.mcp_servers[name]).expect("hash");
            cache.servers.insert(
                name.to_string(),
                ServerCacheEntry {
                    config_hash: hash,
                    tools: vec![CachedTool {
                        name: "search".to_string(),
                        description: None,
                        input_schema: None,
                        ..Default::default()
                    }],
                    cached_at: now_ms(),
                    ..Default::default()
                },
            );
        }
        let all = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        assert_eq!(all.len(), 2);
        let filtered = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::from(["alpha".to_string()]),
        );
        let names: Vec<&str> = filtered.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["beta_search"]);
    }

    /// #432 / R7.2.5.1：描述与上游 `buildProxyDescription` @ v2.32.1
    /// (`10a45367`) 逐字节一致（golden 由上游函数直接生成）。
    #[test]
    fn proxy_description_matches_upstream_bytes() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/proxy_description_cases.json"
        ))
        .expect("fixture parses");
        for case in fixture["cases"].as_array().expect("cases") {
            let config_value = case["config"].clone();
            let mut config = McpConfig::default();
            if let Some(servers) = config_value.get("mcpServers").and_then(Value::as_object) {
                for (name, value) in servers {
                    config.mcp_servers.insert(
                        name.clone(),
                        ServerEntry(value.as_object().cloned().unwrap_or_default()),
                    );
                }
            }
            assert_eq!(
                build_proxy_description(&config),
                case["description"].as_str().unwrap_or_default(),
                "case {}",
                case["name"]
            );
        }
    }

    /// #432 / R7.2.5.1（`__tests__/direct-tools.test.ts`）：描述是 config 的
    /// 纯函数——同一 config 反复调用逐字节相同，且不含工具数/instructions。
    #[test]
    fn proxy_description_is_pure_config_function() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "alpha".to_string(),
            ServerEntry(
                json!({ "command": "node" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        config.mcp_servers.insert(
            "beta".to_string(),
            ServerEntry(
                json!({ "command": "node", "disabled": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let first = build_proxy_description(&config);
        let second = build_proxy_description(&config);
        assert_eq!(first, second);
        assert!(first.contains("\nServers: alpha\n"), "desc: {first}");
        assert!(
            !first.contains("alpha ("),
            "no per-server tool counts: {first}"
        );
        assert!(
            first.contains("Disabled servers (enable with /mcp enable <server> and /reload): beta"),
            "desc: {first}"
        );
        assert!(!first.contains("Direct tools available"), "desc: {first}");
        assert!(!first.contains("Server instructions"), "desc: {first}");

        // Config change is the only input that may change the bytes.
        config.mcp_servers.shift_remove("beta");
        let third = build_proxy_description(&config);
        assert_ne!(first, third);
        assert!(!third.contains("Disabled servers"));
    }

    #[test]
    fn direct_tool_definition_shape() {
        let mut s = spec("search", "demo");
        s.input_schema = Some(json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "additionalProperties": false,
            "properties": { "q": { "type": "string" } },
        }));
        let definition = direct_tool_definition(&s);
        assert_eq!(definition["definition"]["label"], json!("MCP: search"));
        assert_eq!(
            definition["definition"]["promptSnippet"],
            json!("search description")
        );
        let params = &definition["definition"]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(params["type"], json!("object"));
    }

    /// FR-P1-01: env override selects servers/tools above per-server config.
    /// `MCP_DIRECT_TOOLS` selectors beat `definition.directTools`.
    fn cache_for_server(config: &McpConfig, server: &str) -> ServerCacheEntry {
        let definition = &config.mcp_servers[server];
        let hash = crate::cache::compute_server_hash(definition).expect("hash");
        ServerCacheEntry {
            config_hash: hash,
            tools: vec![
                CachedTool {
                    name: "search".to_string(),
                    description: Some("Search things".to_string()),
                    input_schema: None,
                    ..Default::default()
                },
                CachedTool {
                    name: "create".to_string(),
                    description: Some("Create things".to_string()),
                    input_schema: None,
                    ..Default::default()
                },
            ],
            resources: vec![],
            cached_at: now_ms(),
            ..Default::default()
        }
    }

    #[test]
    fn env_override_selects_specific_server_over_per_server_config() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "alpha".to_string(),
            ServerEntry(
                json!({ "command": "x", "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        config.mcp_servers.insert(
            "beta".to_string(),
            ServerEntry(
                json!({ "command": "y", "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        cache
            .servers
            .insert("alpha".to_string(), cache_for_server(&config, "alpha"));
        cache
            .servers
            .insert("beta".to_string(), cache_for_server(&config, "beta"));

        // env selects only alpha → beta excluded even though directTools:true
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            Some(&["alpha".to_string()]),
            &HashSet::new(),
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["alpha_search", "alpha_create"]);
    }

    #[test]
    fn env_override_with_server_tool_selector() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({ "command": "x", "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        cache
            .servers
            .insert("demo".to_string(), cache_for_server(&config, "demo"));

        // env selects only demo/search
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            Some(&["demo/search".to_string()]),
            &HashSet::new(),
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["demo_search"]);
    }

    #[test]
    fn global_settings_direct_tools_when_per_server_unset() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({ "command": "x" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        cache
            .servers
            .insert("demo".to_string(), cache_for_server(&config, "demo"));

        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn disabled_server_skipped() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "off".to_string(),
            ServerEntry(
                json!({ "command": "x", "directTools": true, "disabled": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        assert!(specs.is_empty());
    }

    #[test]
    fn direct_tools_false_overrides_global_true() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({ "command": "x", "directTools": false })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::Server,
            None,
            &HashSet::new(),
        );
        assert!(specs.is_empty());
    }

    #[test]
    fn builtin_name_collision_skipped() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "toolPrefix": "none", "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({ "command": "x" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        let mut cache = MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers: Default::default(),
        };
        let definition = &config.mcp_servers["demo"];
        let hash = crate::cache::compute_server_hash(definition).expect("hash");
        cache.servers.insert(
            "demo".to_string(),
            ServerCacheEntry {
                config_hash: hash,
                tools: vec![
                    CachedTool {
                        name: "mcp".to_string(),
                        description: Some("collides".to_string()),
                        ..Default::default()
                    },
                    CachedTool {
                        name: "safe".to_string(),
                        description: Some("safe tool".to_string()),
                        ..Default::default()
                    },
                ],
                cached_at: now_ms(),
                ..Default::default()
            },
        );
        let specs = resolve_direct_tools(
            &config,
            Some(&cache),
            ToolPrefix::None,
            None,
            &HashSet::new(),
        );
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["safe"]);
    }

    #[test]
    fn prompt_snippet_truncated_at_100_chars() {
        let mut s = spec("search", "demo");
        s.description = "a".repeat(150);
        let definition = direct_tool_definition(&s);
        let snippet = definition["definition"]["promptSnippet"]
            .as_str()
            .unwrap_or("");
        // truncateAtWord(text, 100) → ≤100 UTF-16 units + optional "..." suffix
        // (upstream index.ts:158 `truncateAtWord(directTool.description, 100)`)
        let without_suffix = snippet.strip_suffix("...").unwrap_or(snippet);
        assert!(
            without_suffix.chars().count() <= 100,
            "snippet body must be ≤100 chars: {}",
            without_suffix.chars().count()
        );
    }

    #[test]
    fn empty_description_uses_fallback_snippet() {
        let mut s = spec("search", "demo");
        s.description = String::new();
        let definition = direct_tool_definition(&s);
        assert_eq!(
            definition["definition"]["promptSnippet"],
            json!("MCP tool from demo")
        );
        assert_eq!(
            definition["definition"]["description"],
            json!("(no description)")
        );
    }
    #[test]
    fn prepare_direct_tool_arguments_recovers_one_json_layer() {
        // direct-tools.ts:40-89 @ 10a45367.
        let schema = json!({
            "type": "object",
            "properties": {
                "payload": { "type": "object" },
                "items": { "type": "array" },
                "name": { "type": "string" },
            },
            "required": ["name"],
        });
        // A string-encoded object for a schema-declared object property is
        // recovered in place.
        let args = json!({ "name": "x", "payload": "{\"k\": 1}" });
        let prepared = prepare_direct_tool_arguments(&Some(schema.clone()), &args).expect("valid");
        assert_eq!(prepared["payload"], json!({ "k": 1 }));
        // A string-encoded array for an array property too.
        let args = json!({ "name": "x", "items": "[1, 2]" });
        let prepared = prepare_direct_tool_arguments(&Some(schema.clone()), &args).expect("valid");
        assert_eq!(prepared["items"], json!([1, 2]));
        // A JSON string for a STRING property stays a string (no recovery).
        let args = json!({ "name": "x" });
        let prepared = prepare_direct_tool_arguments(&Some(schema.clone()), &args).expect("valid");
        assert_eq!(prepared["name"], json!("x"));
    }

    #[test]
    fn prepare_direct_tool_arguments_rejects_schema_violations() {
        let schema = json!({
            "type": "object",
            "properties": { "count": { "type": "number" } },
            "required": ["count"],
        });
        let error = prepare_direct_tool_arguments(&Some(schema.clone()), &json!({ "wrong": true }))
            .unwrap_err();
        assert!(
            error
                .starts_with("MCP direct tool arguments do not match the advertised input schema:"),
            "{error}"
        );
        // The envelope keeps the TypeBox shape: issues/total/truncated.
        let tail = error
            .split_once("advertised input schema: ")
            .map(|(_, tail)| tail)
            .unwrap_or_default();
        let parsed: Value = serde_json::from_str(tail).expect("envelope is JSON");
        let issues = parsed["issues"].as_array().expect("issues");
        assert!(!issues.is_empty());
        // Real instancePath/keyword from the validator (round-1 O4): the
        // missing required property surfaces as required at the root.
        let issue = &issues[0];
        assert_eq!(issue["instancePath"], json!("/"), "{issue}");
        assert_eq!(issue["keyword"], json!("required"), "{issue}");
        assert!(parsed
            .get("total")
            .is_some_and(|v| v.as_u64().is_some_and(|t| t > 0)));
        assert!(parsed.get("truncated").is_some());
        // Non-object schemas pass through untouched (no validation).
        let passthrough = prepare_direct_tool_arguments(
            &Some(json!({ "type": "string" })),
            &json!({ "anything": 1 }),
        )
        .expect("non-object schema passes");
        assert_eq!(passthrough, json!({ "anything": 1 }));
    }
}
