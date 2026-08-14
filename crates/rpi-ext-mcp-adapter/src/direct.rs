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
//! (FR-P1-08) are wired into the direct executor (TE-D09/TE-D11); tool
//! approval (FR-P1-07) and MCP UI sessions (P2) are not.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{json, Value};
use tracing::warn;

use crate::cache::{is_server_cache_valid, MetadataCache};
use crate::metadata::{
    format_tool_name, is_tool_allowed, resolve_tool_prefix, resource_name_to_tool_name, McpConfig,
    ToolPrefix,
};
use crate::utils::truncate_at_word;

/// `BUILTIN_NAMES` (direct-tools.ts:25): direct tools may not shadow these.
const BUILTIN_NAMES: [&str; 8] = ["read", "bash", "edit", "write", "grep", "find", "ls", "mcp"];
/// `DIRECT_TOOLS_ADVISORY_THRESHOLD` (direct-tools.ts:27).
pub const DIRECT_TOOLS_ADVISORY_THRESHOLD: usize = 75;
/// `INSTRUCTIONS_SNIPPET_LENGTH` (direct-tools.ts:26) — used by the proxy
/// description builder.
pub const INSTRUCTIONS_SNIPPET_LENGTH: usize = 150;

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

/// `resolveDirectTools` (direct-tools.ts:114-208).
pub fn resolve_direct_tools(
    config: &McpConfig,
    cache: Option<&MetadataCache>,
    prefix: ToolPrefix,
    env_override: Option<&[String]>,
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

    if specs.len() >= DIRECT_TOOLS_ADVISORY_THRESHOLD {
        warn!(
            count = specs.len(),
            "MCP: many direct tools resolved; each direct tool adds prompt context — prefer targeted sets of 5-20 tools"
        );
    }
    specs
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
            // renderMcpToolResult for direct tools (index.ts:161-162): the
            // shared MCP result renderer handles collapse on the host side.
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

/// `buildProxyDescription` (direct-tools.ts:210-290), full fidelity.
pub fn build_proxy_description(
    config: &McpConfig,
    cache: Option<&MetadataCache>,
    direct_specs: &[DirectToolSpec],
) -> String {
    let prefix = config.global_tool_prefix();
    let mut desc = "MCP gateway — server status, tool search/describe, auth, and single MCP tool calls. When one request needs several MCP calls with logic between them, use mcpScript. Non-MCP Pi tools should be called directly, not through mcp.\n".to_string();

    let mut direct_by_server: IndexMap<&str, usize> = IndexMap::new();
    for spec in direct_specs {
        *direct_by_server
            .entry(spec.server_name.as_str())
            .or_insert(0) += 1;
    }
    if !direct_by_server.is_empty() {
        let parts: Vec<String> = direct_by_server
            .iter()
            .map(|(server, count)| format!("{server} ({count})"))
            .collect();
        desc.push_str(&format!(
            "\nDirect tools available (call as normal tools): {}\n",
            parts.join(", ")
        ));
    }

    let mut server_summaries: Vec<String> = Vec::new();
    for (server_name, definition) in &config.mcp_servers {
        if definition.is_disabled() {
            continue;
        }
        let entry = cache.and_then(|c| c.servers.get(server_name));
        let effective_prefix = resolve_tool_prefix(Some(definition), prefix);
        let tool_count = entry
            .map(|e| {
                e.tools
                    .iter()
                    .filter(|tool| {
                        is_tool_allowed(
                            &tool.name,
                            server_name,
                            effective_prefix,
                            definition.include_tools(),
                            definition.exclude_tools(),
                        )
                    })
                    .count()
            })
            .unwrap_or(0);
        let resource_count = if definition.exposes_resources() {
            entry
                .map(|e| {
                    e.resources
                        .iter()
                        .filter(|resource| {
                            let base_name =
                                format!("read_{}", resource_name_to_tool_name(&resource.name));
                            is_tool_allowed(
                                &base_name,
                                server_name,
                                effective_prefix,
                                definition.include_tools(),
                                definition.exclude_tools(),
                            )
                        })
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };
        let total_items = tool_count + resource_count;
        if total_items == 0 {
            continue;
        }
        let direct_count = direct_by_server
            .get(server_name.as_str())
            .copied()
            .unwrap_or(0);
        let proxy_count = total_items.saturating_sub(direct_count);
        if proxy_count > 0 {
            server_summaries.push(format!("{server_name} ({proxy_count} tools)"));
        }
    }
    if !server_summaries.is_empty() {
        desc.push_str(&format!("\nServers: {}\n", server_summaries.join(", ")));
    }

    let disabled: Vec<&str> = config
        .mcp_servers
        .iter()
        .filter(|(_, d)| d.is_disabled())
        .map(|(name, _)| name.as_str())
        .collect();
    if !disabled.is_empty() {
        desc.push_str(&format!(
            "\nDisabled servers (enable with /mcp enable <server> and /reload): {}\n",
            disabled.join(", ")
        ));
    }

    let mut instruction_summaries: Vec<String> = Vec::new();
    for (server_name, definition) in &config.mcp_servers {
        if definition.is_disabled() {
            continue;
        }
        let instructions = cache
            .and_then(|c| c.servers.get(server_name))
            .and_then(|e| e.instructions.as_deref());
        let Some(instructions) = instructions else {
            continue;
        };
        let collapsed: String = instructions
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let snippet = truncate_at_word(&collapsed, INSTRUCTIONS_SNIPPET_LENGTH);
        instruction_summaries.push(format!("  {server_name}: {snippet}"));
    }
    if !instruction_summaries.is_empty() {
        desc.push_str(&format!(
            "\nServer instructions (truncated - full text via mcp({{ instructions: \"name\" }})):\n{}\n",
            instruction_summaries.join("\n")
        ));
    }

    desc.push_str("\nUsage:\n");
    desc.push_str("  mcp({ })                              → Show server status\n");
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
/// P1-wave cut: no approval gate (FR-P1-07), no UI sessions (P2). Auto-auth
/// (FR-P1-04) and session recovery (FR-P1-08) are wired in (TE-D09/TE-D11).
pub async fn execute_direct_tool(
    runtime: &crate::proxy::McpRuntime,
    spec: &DirectToolSpec,
    params: &Value,
) -> Value {
    let config = &runtime.config;
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

    let guard_options = crate::guard::resolve_guard_options(config.settings.as_ref());
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
                let guarded = crate::guard::guard_mcp_output(content, &guard_options);
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
            let guarded = crate::guard::guard_mcp_output(content, &guard_options);
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
        let specs = resolve_direct_tools(&config, Some(&cache), ToolPrefix::Server, None);
        let names: Vec<&str> = specs.iter().map(|s| s.prefixed_name.as_str()).collect();
        assert_eq!(names, ["demo_search", "demo_read_doc"]);
        assert_eq!(specs[1].resource_uri.as_deref(), Some("mcp://demo/doc"));

        // per-server list filter
        config.mcp_servers.get_mut("demo").map(|d| {
            d.as_map_mut()
                .insert("directTools".to_string(), json!(["search"]))
        });
        let specs = resolve_direct_tools(&config, Some(&cache), ToolPrefix::Server, None);
        assert_eq!(specs.len(), 1);
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

        let specs = resolve_direct_tools(&config, Some(&cache), ToolPrefix::Server, None);
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
        let specs = resolve_direct_tools(&config, Some(&cache), ToolPrefix::Server, None);
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
        let specs = resolve_direct_tools(&config, Some(&cache), ToolPrefix::Server, None);
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
        let specs = resolve_direct_tools(&config, Some(&cache), ToolPrefix::None, None);
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
}
