//! The `mcp` proxy tool: parameter surface, dispatch priority, init gating,
//! and the search / describe / call / status / list / instructions / connect
//! modes (FR-P0-02/03/04/05/06/11/12, design §2.2).
//!
//! Port of `proxy-modes.ts` + the `registerProxyTool` part of `index.ts` +
//! the state/init helpers of `init.ts` + the content transforms of
//! `tool-registrar.ts` + `error-signal.ts` @ pi-mcp-adapter v2.24.0
//! (3d953f90).
//!
//! P0 scope cuts (all documented in the requirements):
//! - Output guard is P1; the guard-disabled code path is reproduced exactly
//!   (affixes, empty fallback, raw `details.mcpResult`).
//! - OAuth manual actions (`action: auth-start/auth-complete`) return
//!   `not_supported` text guidance; `ui-messages` is P2. Auto-auth
//!   (`settings.autoAuth`) and Streamable HTTP session recovery are wired
//!   into the connect/call paths (TE-D09/TE-D11).
//! - Tool approval gates and MCP UI sessions (P2) are absent.
//! - [ABI] Upstream *throws* on invalid `args` JSON; the native ABI has no
//!   toolExecute throw channel, so the same message is returned as a normal
//!   tool result with `details.error: "invalid_args"` and re-flagged via the
//!   plugin's own `tool_result` hook (deviation TE-D04).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared};
use futures::StreamExt;
use indexmap::IndexMap;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::cache::{
    compute_server_hash, get_metadata_cache_path, is_server_cache_valid, load_metadata_cache,
    reconstruct_prompt_metadata, reconstruct_tool_metadata, save_metadata_cache,
    serialize_resources, serialize_tools, MetadataCache, PromptMetadata, ServerCacheEntry,
};
use crate::config::load_mcp_config;
use crate::lifecycle::{
    FailureTracker, LifecycleManager, LifecycleMode, DEFAULT_IDLE_TIMEOUT_MINUTES,
};
use crate::manager::{ConnectionStatus, McpServerManager, ServerConnection};
use crate::metadata::{
    build_tool_metadata, find_tool_by_name, format_schema, get_server_prefix, McpConfig,
    McpResource, McpTool, ServerEntry, ToolMetadata,
};
use crate::search::{
    paginate, rank_suggestions, rank_tool_matches, resolve_search_keywords, SearchState,
};
use crate::session_recovery;
use crate::tsshape::render_ts_shape;
use crate::utils::truncate_at_word;

/// `INIT_WAIT_TIMEOUT_MS` (index.ts:38).
pub const INIT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// `MAX_REGEX_SEARCH_QUERY_LENGTH` (proxy-modes.ts:27).
const MAX_REGEX_SEARCH_QUERY_LENGTH: usize = 256;
/// `INSTRUCTIONS_PREVIEW_LENGTH` (proxy-modes.ts:28).
const INSTRUCTIONS_PREVIEW_LENGTH: usize = 300;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ============================================================================
// Runtime state (state.ts `McpExtensionState`, P0 fields)
// ============================================================================

/// The P0 runtime state (upstream `McpExtensionState`, minus UI/OAuth/
/// direct-tools fields).
pub struct McpRuntime {
    pub config: McpConfig,
    pub manager: Arc<McpServerManager>,
    pub lifecycle: Arc<LifecycleManager>,
    pub failures: Arc<FailureTracker>,
    pub owner_cancel: CancellationToken,
    /// Insertion-ordered like the upstream `Map` (search iteration order).
    pub tool_metadata: Mutex<IndexMap<String, Vec<ToolMetadata>>>,
    pub resource_counts: Mutex<HashMap<String, usize>>,
    pub prompt_metadata: Mutex<HashMap<String, Vec<PromptMetadata>>>,
    pub server_instructions: Mutex<HashMap<String, String>>,
    pub cache_path: PathBuf,
    /// `state.onToolMetadataUpdated` (index.ts:313-321): set by the plugin
    /// after init completes; fired with (server, reason) at metadata
    /// refresh points. The hook itself decides about freezeDirectTools.
    pub on_metadata_updated: Mutex<Option<MetadataUpdatedHook>>,
}

/// Callback fired at metadata refresh points (index.ts:313-321).
type MetadataUpdatedHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

// SearchState holds `&[(String, Vec<ToolMetadata>)]`; provide it via a
// snapshot helper to keep lock times minimal.
fn search_state_snapshot(state: &McpRuntime) -> (McpConfig, Vec<(String, Vec<ToolMetadata>)>) {
    let metadata = state
        .tool_metadata
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (state.config.clone(), metadata)
}

fn wire_tool(value: &Value) -> Option<McpTool> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(McpTool {
        name: name.to_string(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_schema: value.get("inputSchema").cloned(),
    })
}

fn wire_tools(values: &[Value]) -> Vec<McpTool> {
    values.iter().filter_map(wire_tool).collect()
}

fn wire_resources(values: &[Value]) -> Vec<McpResource> {
    values
        .iter()
        .filter_map(|value| {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let uri = value
                .get("uri")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            Some(McpResource {
                uri: uri.to_string(),
                name: name.to_string(),
                description: value
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

// ============================================================================
// init (init.ts `initializeMcp`, P0 cut)
// ============================================================================

/// `initializeMcp` (init.ts:91-413), P0 cut: config load, lifecycle
/// registration, cache restore, startup connects (bootstrap-all when the
/// cache file is absent; else eager/keep-alive only, concurrency limit 10),
/// health checks. UI notifications, OAuth runtime, direct-tools bootstrap
/// and the status event bus are P1/P2.
pub async fn initialize_mcp(
    cwd: &Path,
    config_path: Option<&str>,
    cache_path: Option<PathBuf>,
) -> Arc<McpRuntime> {
    let config = load_mcp_config(config_path, cwd);
    let cache_path = cache_path.unwrap_or_else(get_metadata_cache_path);
    let owner_cancel = CancellationToken::new();

    let manager = McpServerManager::new(Some(cwd.to_string_lossy().into_owned()));
    manager.set_runtime_cancel(owner_cancel.clone());
    let request_timeout_ms = config
        .settings
        .as_ref()
        .and_then(|s| s.get("requestTimeoutMs"))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0);
    manager.set_default_request_timeout(
        request_timeout_ms.map(|ms| std::time::Duration::from_millis(ms as u64)),
    );
    let lifecycle = LifecycleManager::new(manager.clone(), owner_cancel.clone());
    let idle_setting = config
        .settings
        .as_ref()
        .and_then(|s| s.get("idleTimeout"))
        .and_then(Value::as_f64)
        .map(|v| v as u64);
    lifecycle.set_global_idle_timeout_minutes(idle_setting.unwrap_or(DEFAULT_IDLE_TIMEOUT_MINUTES));

    let state = Arc::new(McpRuntime {
        config: config.clone(),
        manager: manager.clone(),
        lifecycle: lifecycle.clone(),
        failures: Arc::new(FailureTracker::new()),
        owner_cancel: owner_cancel.clone(),
        tool_metadata: Mutex::new(IndexMap::new()),
        resource_counts: Mutex::new(HashMap::new()),
        prompt_metadata: Mutex::new(HashMap::new()),
        server_instructions: Mutex::new(HashMap::new()),
        cache_path: cache_path.clone(),
        on_metadata_updated: Mutex::new(None),
    });

    let enabled: Vec<(&String, &ServerEntry)> = config
        .mcp_servers
        .iter()
        .filter(|(_, d)| !d.is_disabled())
        .collect();
    if enabled.is_empty() {
        return state;
    }

    // Cache bootstrap semantics (init.ts:215-226).
    let cache_file_exists = cache_path.exists();
    let mut cache = load_metadata_cache(&cache_path);
    let bootstrap_all = if !cache_file_exists {
        let _ = save_metadata_cache(
            &cache_path,
            &MetadataCache {
                version: crate::cache::CACHE_VERSION,
                servers: Default::default(),
            },
        );
        true
    } else {
        if cache.is_none() {
            let empty = MetadataCache {
                version: crate::cache::CACHE_VERSION,
                servers: Default::default(),
            };
            let _ = save_metadata_cache(&cache_path, &empty);
            cache = Some(empty);
        }
        false
    };

    let prefix = config.global_tool_prefix();
    for (name, definition) in &enabled {
        let mode = LifecycleMode::of(definition);
        let idle_override = match definition.get("idleTimeout").and_then(Value::as_u64) {
            Some(v) => Some(v),
            None if mode.persists_after_first_spawn() => Some(0),
            None => None,
        };
        lifecycle.register_server(name, definition, idle_override);
        if mode == LifecycleMode::KeepAlive {
            lifecycle.mark_keep_alive(name, definition);
        }

        let cached = cache
            .as_ref()
            .and_then(|c| c.servers.get(*name))
            .filter(|entry| {
                is_server_cache_valid(entry, definition, crate::cache::CACHE_MAX_AGE_MS, now_ms())
            });
        if let Some(cached) = cached {
            let metadata = reconstruct_tool_metadata(name, cached, prefix, definition);
            state
                .tool_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((*name).clone(), metadata);
            state
                .resource_counts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((*name).clone(), cached.resources.len());
            if let Some(prompts) = &cached.prompts {
                if !prompts.is_empty() {
                    let reconstructed =
                        reconstruct_prompt_metadata(name, prompts, prefix, Some(definition));
                    state
                        .prompt_metadata
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert((*name).clone(), reconstructed);
                }
            }
            if let Some(instructions) = &cached.instructions {
                state
                    .server_instructions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((*name).clone(), instructions.clone());
            }
        }
    }

    let startup: Vec<(String, ServerEntry)> = enabled
        .iter()
        .filter(|(_, definition)| {
            bootstrap_all
                || matches!(
                    LifecycleMode::of(definition),
                    LifecycleMode::KeepAlive | LifecycleMode::Eager
                )
        })
        .map(|(name, definition)| ((*name).clone(), (*definition).clone()))
        .collect();

    // `parallelLimit(startupServers, 10, ...)` (init.ts:271-286).
    let connect_results: Vec<(String, Result<Arc<ServerConnection>, String>)> =
        futures::stream::iter(startup.into_iter().map(|(name, definition)| {
            let manager = manager.clone();
            let cancel = owner_cancel.clone();
            async move {
                let result = tokio::select! {
                    _ = cancel.cancelled() => Err("aborted".to_string()),
                    result = manager.connect(&name, &definition) => result.map_err(|e| e.to_string()),
                };
                (name, result)
            }
        }))
        .buffer_unordered(10)
        .collect()
        .await;

    for (name, result) in connect_results {
        match result {
            Ok(connection) => {
                if connection.status() == ConnectionStatus::NeedsAuth {
                    continue;
                }
                update_server_metadata(&state, &name);
                update_metadata_cache(&state, &name);
                mark_keep_alive_after_connect(&state, &name);
            }
            Err(message) => {
                if message == "aborted" {
                    continue;
                }
                state.failures.record(&name, &message, owner_cancel.clone());
            }
        }
    }

    let reconnect_state = state.clone();
    lifecycle.set_reconnect_callback(Arc::new(move |name| {
        update_server_metadata(&reconnect_state, name);
        update_metadata_cache(&reconnect_state, name);
        notify_metadata_updated(&reconnect_state, name, "lifecycle-reconnect");
        reconnect_state.failures.clear(name);
    }));
    let failure_state = state.clone();
    let failure_cancel = owner_cancel.clone();
    lifecycle.set_reconnect_failure_callback(Arc::new(move |name, message| {
        failure_state
            .failures
            .record(name, message, failure_cancel.clone());
    }));

    lifecycle.start_health_checks();
    state
}

/// `notifyToolMetadataUpdated` (init.ts:497-510).
pub fn notify_metadata_updated(state: &McpRuntime, server_name: &str, reason: &str) {
    let hook = state
        .on_metadata_updated
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(server_name, reason);
    }
}

/// `markKeepAliveAfterConnect` (init.ts:415-421).
pub fn mark_keep_alive_after_connect(state: &McpRuntime, server_name: &str) {
    let Some(definition) = state.config.mcp_servers.get(server_name) else {
        return;
    };
    if definition.is_disabled() {
        return;
    }
    if LifecycleMode::of(definition) == LifecycleMode::LazyKeepAlive {
        state.lifecycle.mark_keep_alive(server_name, definition);
    }
}

/// `updateServerMetadata` (init.ts:423-452).
pub fn update_server_metadata(state: &McpRuntime, server_name: &str) {
    let Some(connection) = state.manager.get_connection(server_name) else {
        return;
    };
    if connection.status() != ConnectionStatus::Connected {
        return;
    }
    let Some(definition) = state.config.mcp_servers.get(server_name) else {
        return;
    };
    if definition.is_disabled() {
        state
            .tool_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(server_name);
        state
            .resource_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server_name);
        state
            .prompt_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server_name);
        state
            .server_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server_name);
        return;
    }
    let prefix = state.config.global_tool_prefix();
    let tools = wire_tools(&connection.tools);
    let resources = wire_resources(&connection.resources);
    let result = build_tool_metadata(&tools, &resources, definition, server_name, prefix);
    state
        .tool_metadata
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(server_name.to_string(), result.metadata);
    state
        .resource_counts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(server_name.to_string(), connection.resources.len());
    if !connection.prompt_discovery_failed {
        let prompts = crate::cache::reconstruct_prompt_metadata(
            server_name,
            &prompt_values_to_cached(&connection.prompts),
            prefix,
            Some(definition),
        );
        state
            .prompt_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_name.to_string(), prompts);
    }
    match &connection.instructions {
        Some(instructions) => state
            .server_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_name.to_string(), instructions.clone()),
        None => state
            .server_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server_name),
    };
}

fn prompt_values_to_cached(prompts: &[Value]) -> Vec<crate::cache::CachedPrompt> {
    prompts
        .iter()
        .filter_map(|p| {
            let name = p
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            Some(crate::cache::CachedPrompt {
                name: name.to_string(),
                title: p.get("title").and_then(Value::as_str).map(str::to_string),
                description: p
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                arguments: None,
            })
        })
        .collect()
}

/// `updateMetadataCache` (init.ts:454-495).
pub fn update_metadata_cache(state: &McpRuntime, server_name: &str) {
    let Some(connection) = state.manager.get_connection(server_name) else {
        return;
    };
    if connection.status() != ConnectionStatus::Connected {
        return;
    }
    let Some(definition) = state.config.mcp_servers.get(server_name) else {
        return;
    };
    if definition.is_disabled() {
        return;
    }
    let Ok(config_hash) = compute_server_hash(definition) else {
        return;
    };
    let existing = load_metadata_cache(&state.cache_path);
    let existing_entry = existing.as_ref().and_then(|c| c.servers.get(server_name));

    let tools = serialize_tools(&wire_tools(&connection.tools));
    let mut resources = if definition.exposes_resources() {
        serialize_resources(&wire_resources(&connection.resources))
    } else {
        Vec::new()
    };
    let prompts = if connection.prompt_discovery_failed {
        if existing_entry.is_some_and(|e| e.config_hash == config_hash) {
            existing_entry.and_then(|e| e.prompts.clone())
        } else {
            None
        }
    } else {
        Some(prompt_values_to_cached(&connection.prompts))
    };
    if definition.exposes_resources()
        && resources.is_empty()
        && existing_entry.is_some_and(|e| !e.resources.is_empty() && e.config_hash == config_hash)
    {
        resources = existing_entry
            .map(|e| e.resources.clone())
            .unwrap_or_default();
    }

    let entry = ServerCacheEntry {
        config_hash,
        tools,
        resources,
        prompts,
        instructions: connection.instructions.clone(),
        cached_at: now_ms(),
    };
    let mut servers = IndexMap::new();
    servers.insert(server_name.to_string(), entry);
    let _ = save_metadata_cache(
        &state.cache_path,
        &MetadataCache {
            version: crate::cache::CACHE_VERSION,
            servers,
        },
    );
}

/// `flushMetadataCache` (init.ts:512-518).
pub fn flush_metadata_cache(state: &McpRuntime) {
    for (name, connection) in state.manager.get_all_connections() {
        if connection.status() == ConnectionStatus::Connected {
            update_metadata_cache(state, &name);
        }
    }
}

/// `lazyConnect` (init.ts:569-614), P0 cut (no UI status).
pub async fn lazy_connect(state: &McpRuntime, server_name: &str) -> bool {
    if state.owner_cancel.is_cancelled() {
        return false;
    }
    if let Some(connection) = state.manager.get_connection(server_name) {
        match connection.status() {
            ConnectionStatus::NeedsAuth => return false,
            ConnectionStatus::Connected => {
                update_server_metadata(state, server_name);
                mark_keep_alive_after_connect(state, server_name);
                return true;
            }
            ConnectionStatus::Closed => {}
        }
    }
    if state.failures.failure_age_seconds(server_name).is_some() {
        return false;
    }
    let Some(definition) = state.config.mcp_servers.get(server_name) else {
        return false;
    };
    if definition.is_disabled() {
        return false;
    }
    match state.manager.connect(server_name, definition).await {
        Ok(connection) => {
            if connection.status() == ConnectionStatus::NeedsAuth {
                return false;
            }
            state.failures.clear(server_name);
            update_server_metadata(state, server_name);
            update_metadata_cache(state, server_name);
            notify_metadata_updated(state, server_name, "lazy-connect");
            mark_keep_alive_after_connect(state, server_name);
            true
        }
        Err(error) => {
            state
                .failures
                .record(server_name, &error.to_string(), state.owner_cancel.clone());
            debug!(server = %server_name, "MCP: lazy connect failed");
            false
        }
    }
}

// ============================================================================
// Content transforms (tool-registrar.ts)
// ============================================================================

/// `transformMcpContent` (tool-registrar.ts:187-230). Binary resource
/// materialization to temp files follows the same shape, minus the
/// session-scoped retry machinery (single per-runtime temp dir).
pub fn transform_mcp_content(content: &[Value]) -> Vec<Value> {
    content
        .iter()
        .map(|c| {
            let kind = c.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "text" => json!({
                    "type": "text",
                    "text": c.get("text").and_then(Value::as_str).unwrap_or(""),
                }),
                "image" => json!({
                    "type": "image",
                    "data": c.get("data").and_then(Value::as_str).unwrap_or(""),
                    "mimeType": c.get("mimeType").and_then(Value::as_str).unwrap_or("image/png"),
                }),
                "resource" => {
                    let resource = c.get("resource");
                    let uri = resource
                        .and_then(|r| r.get("uri"))
                        .and_then(Value::as_str)
                        .unwrap_or("(no URI)");
                    if let Some(blob) = resource
                        .and_then(|r| r.get("blob"))
                        .and_then(Value::as_str)
                    {
                        return json!({
                            "type": "text",
                            "text": materialize_binary_resource(uri, blob, resource.and_then(|r| r.get("mimeType")).and_then(Value::as_str)),
                        });
                    }
                    let text = resource
                        .and_then(|r| r.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| resource.map(|r| r.to_string()))
                        .unwrap_or_else(|| "(no content)".to_string());
                    json!({ "type": "text", "text": format!("[Resource: {uri}]\n{text}") })
                }
                "resource_link" => {
                    let name = c
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| c.get("uri").and_then(Value::as_str))
                        .unwrap_or("unknown");
                    let uri = c.get("uri").and_then(Value::as_str).unwrap_or("(no URI)");
                    json!({ "type": "text", "text": format!("[Resource Link: {name}]\nURI: {uri}") })
                }
                "audio" => json!({
                    "type": "text",
                    "text": format!("[Audio content: {}]", c.get("mimeType").and_then(Value::as_str).unwrap_or("audio/*")),
                }),
                _ => json!({ "type": "text", "text": c.to_string() }),
            }
        })
        .collect()
}

/// `MAX_BINARY_RESOURCE_BYTES` (tool-registrar.ts:10).
const MAX_BINARY_RESOURCE_BYTES: usize = 10 * 1024 * 1024;

/// `materializeBinaryResource` (tool-registrar.ts:133-174), simplified:
/// decoded-size cap + 0600 temp file under a per-process directory.
fn materialize_binary_resource(uri: &str, blob: &str, mime: Option<&str>) -> String {
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .unwrap_or_default();
    let mime = mime.unwrap_or("application/octet-stream");
    if decoded.len() > MAX_BINARY_RESOURCE_BYTES {
        return [
            format!("[Resource: {uri}]"),
            "Binary content omitted: decoded size exceeds 10 MiB".to_string(),
            format!("MIME type: {mime}"),
        ]
        .join("\n");
    }
    let dir = std::env::temp_dir().join(format!("rpi-mcp-resource-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return omit_binary(uri, mime, "could not be saved");
    }
    let path = dir.join(format!("resource-{}.bin", now_ms()));
    let write = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(&decoded)
        });
    if write.is_err() {
        return omit_binary(uri, mime, "could not be saved");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    [
        format!("[Resource: {uri}]"),
        format!("Binary content saved to {}", path.display()),
        format!("MIME type: {mime}"),
    ]
    .join("\n")
}

fn omit_binary(uri: &str, mime: &str, reason: &str) -> String {
    [
        format!("[Resource: {uri}]"),
        format!("Binary content omitted: {reason}"),
        format!("MIME type: {mime}"),
    ]
    .join("\n")
}

/// `resolveMcpResultContent` (tool-registrar.ts:236-245): content blocks,
/// falling back to `structuredContent` (pretty JSON).
pub fn resolve_mcp_result_content(result: &Value) -> Vec<Value> {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .map(|a| transform_mcp_content(a))
        .unwrap_or_default();
    if !blocks.is_empty() {
        return blocks;
    }
    if let Some(structured) = result.get("structuredContent") {
        if !structured.is_null() {
            let text =
                serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
            return vec![json!({ "type": "text", "text": text })];
        }
    }
    Vec::new()
}

/// `transformMcpResourceContents` (tool-registrar.ts:179-185).
pub fn transform_mcp_resource_contents(contents: &[Value]) -> Vec<Value> {
    contents
        .iter()
        .map(|resource| {
            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                return json!({ "type": "text", "text": text });
            }
            if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("(no URI)");
                let mime = resource.get("mimeType").and_then(Value::as_str);
                return json!({
                    "type": "text",
                    "text": materialize_binary_resource(uri, blob, mime),
                });
            }
            json!({ "type": "text", "text": resource.to_string() })
        })
        .collect()
}

// ============================================================================
// error-signal.ts
// ============================================================================

/// `toolErrorOverride` (error-signal.ts:13-21): flip `isError` for results
/// whose details carry `tool_error` / `call_failed`. Consumed by the
/// plugin's `tool_result` event handler (ABI hook verified by TE01:
/// `ToolResultEventResult.isError`, runner partial-patch chaining).
pub fn tool_error_override(details: &Value) -> Option<Value> {
    let code = details.get("error").and_then(Value::as_str)?;
    if code == "tool_error" || code == "call_failed" {
        return Some(json!({ "isError": true }));
    }
    None
}

// ============================================================================
// Proxy tool surface (index.ts registerProxyTool)
// ============================================================================

/// The `mcp` tool parameter schema (index.ts:699-718), hand-written JSON
/// Schema equivalent to the upstream TypeBox object (all fields optional).
pub fn tool_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tool": { "type": "string", "description": "Tool name to call (e.g., 'xcodebuild_list_sims')" },
            "args": {
                "anyOf": [
                    { "type": "string", "description": "Arguments as a JSON string (e.g., '{\"key\": \"value\"}')" },
                    { "type": "object", "additionalProperties": true, "description": "Arguments as a JSON object (e.g., { \"key\": \"value\" })" }
                ],
                "description": "Tool arguments as a JSON object, or as a JSON string encoding one"
            },
            "connect": { "type": "string", "description": "Server name to connect (lazy connect + metadata refresh)" },
            "describe": { "type": "string", "description": "Tool name to describe (shows parameters)" },
            "instructions": { "type": "string", "description": "Server name to show that server's usage instructions" },
            "search": { "type": "string", "description": "Search tools by name/description" },
            "regex": { "type": "boolean", "description": "Treat search as regex (default: substring match)" },
            "includeSchemas": { "type": "boolean", "description": "Include parameter schemas in search results (default: true)" },
            "limit": { "type": "number", "minimum": 1, "description": "Maximum search results to return (default: 12)" },
            "offset": { "type": "number", "minimum": 0, "description": "Search result offset (default: 0)" },
            "server": { "type": "string", "description": "Filter to specific server (also disambiguates tool calls)" },
            "action": { "type": "string", "description": "Action: 'ui-messages', 'auth-start', or 'auth-complete'" }
        }
    })
}

fn text_result(text: String, details: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "details": details,
    })
}

fn disabled_result(mode: &str, server_name: &str) -> Value {
    let message = format!(
        "Server \"{server_name}\" is disabled. Run /mcp enable {server_name} and /reload to enable it."
    );
    text_result(
        message.clone(),
        json!({ "mode": mode, "error": "server_disabled", "server": server_name, "message": message }),
    )
}

/// The default needs-auth guidance (proxy-modes.ts:48-54).
pub fn auth_required_message(config: &McpConfig, server_name: &str) -> String {
    let default_message = format!(
        "Server \"{server_name}\" requires OAuth authentication. Run mcp({{ action: \"auth-start\", server: \"{server_name}\" }}) to get a browser URL, or /mcp-auth {server_name} in an interactive local session."
    );
    match config
        .settings
        .as_ref()
        .and_then(|s| s.get("authRequiredMessage"))
        .and_then(Value::as_str)
    {
        Some(template) => template.replace("${server}", server_name),
        None => default_message,
    }
}

/// `getAuthFailedMessage` (proxy-modes.ts:56-62).
pub fn auth_failed_message(config: &McpConfig, server_name: &str, message: &str) -> String {
    match config
        .settings
        .as_ref()
        .and_then(|s| s.get("authRequiredMessage"))
        .and_then(Value::as_str)
    {
        Some(template) => format!(
            "OAuth authentication failed for \"{server_name}\": {message}. {}",
            template.replace("${server}", server_name)
        ),
        None => format!(
            "OAuth authentication failed for \"{server_name}\": {message}. Run mcp({{ action: \"auth-start\", server: \"{server_name}\" }}) to get a browser URL, or /mcp-auth {server_name} in an interactive local session."
        ),
    }
}

/// `attemptAutoAuth` (proxy-modes.ts:98-148, TE-D09): when a connection is
/// `needs-auth` and `settings.autoAuth === true`, attempt automatic OAuth
/// authentication, then close the stale connection, clear the failure and
/// reconnect (the upstream callers do the close/reconnect inline; this port
/// folds it in so every call site shares one code path).
///
/// Returns:
/// - `Ok(true)` — authentication succeeded and the server is now connected.
/// - `Ok(false)` — skipped (autoAuth disabled, server absent/disabled, no
///   OAuth support, or no usable URL) or authenticated but still not
///   connected; callers surface the default needs-auth guidance.
/// - `Err(message)` — authentication was attempted but failed; `message` is
///   the user-facing `getAuthFailedMessage` text.
///
/// Headless behavior (upstream `!state.ui` guard, proxy-modes.ts:121-129):
/// the native plugin has no UI, so `authorization_code` servers fail fast
/// with the manual-auth guidance instead of blocking on the 5-minute
/// localhost callback; only `client_credentials` servers proceed to the
/// token endpoint. Error messages never embed token values (G4).
pub async fn attempt_auto_auth(state: &McpRuntime, server_name: &str) -> Result<bool, String> {
    // proxy-modes.ts:101-103
    let auto_auth = state
        .config
        .settings
        .as_ref()
        .and_then(|s| s.get("autoAuth"))
        .and_then(Value::as_bool)
        == Some(true);
    if !auto_auth {
        return Ok(false);
    }

    // proxy-modes.ts:105-108
    let Some(definition) = state.config.mcp_servers.get(server_name) else {
        return Ok(false);
    };
    if definition.is_disabled() {
        return Ok(false);
    }
    if !crate::manager::supports_oauth(definition) {
        return Ok(false);
    }

    // proxy-modes.ts:110-118 — a URL resolution failure is a *failed*
    // attempt (it means the entry has a malformed url/env expansion).
    let server_url = match crate::utils::resolve_server_url(definition.get("url")) {
        Ok(Some(url)) => url,
        Ok(None) => return Ok(false),
        Err(error) => {
            return Err(auth_failed_message(
                &state.config,
                server_name,
                &error.to_string(),
            ));
        }
    };

    // proxy-modes.ts:120-129 — headless guard: no UI means no browser to
    // complete the authorization-code redirect.
    let grant_type = crate::oauth::configured_grant_type(definition);
    if grant_type != "client_credentials" {
        return Err(auth_required_message(&state.config, server_name));
    }

    // proxy-modes.ts:131-141
    let oauth_dir = state
        .config
        .settings
        .as_ref()
        .and_then(|s| s.get("oauthDir"))
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    let options = crate::oauth::AuthenticateOptions {
        auth_storage_options: crate::oauth::store::AuthStorageOptions {
            base_dir: oauth_dir,
        },
        ..Default::default()
    };

    match crate::oauth::authenticate(server_name, &server_url, definition, &options).await {
        Ok(_) => {
            // Close stale connection, clear failure, reconnect.
            state.manager.close(server_name).await;
            state.failures.clear(server_name);
            match state.manager.connect(server_name, definition).await {
                Ok(conn) if conn.status() == ConnectionStatus::Connected => {
                    state.failures.clear(server_name);
                    update_server_metadata(state, server_name);
                    update_metadata_cache(state, server_name);
                    notify_metadata_updated(state, server_name, "auto-auth");
                    mark_keep_alive_after_connect(state, server_name);
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
        Err(error) => {
            // AdapterError messages carry context strings ("token exchange:
            // HTTP 401"), never token values (G4).
            tracing::warn!(server = %server_name, "MCP: auto-auth failed");
            Err(auth_failed_message(
                &state.config,
                server_name,
                &error.to_string(),
            ))
        }
    }
}

// ============================================================================
// Modes (proxy-modes.ts)
// ============================================================================

/// `executeStatus` (proxy-modes.ts:249-313).
pub fn execute_status(state: &McpRuntime) -> Value {
    let mut servers: Vec<Value> = Vec::new();
    let tool_metadata = state
        .tool_metadata
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (name, definition) in &state.config.mcp_servers {
        let disabled = definition.is_disabled();
        let connection = if disabled {
            None
        } else {
            state.manager.get_connection(name)
        };
        let metadata = if disabled {
            None
        } else {
            tool_metadata.get(name)
        };
        let tool_count = metadata.map_or(0, Vec::len);
        let failed_ago = if disabled {
            None
        } else {
            state.failures.failure_age_seconds(name)
        };
        let mut status = if disabled {
            "disabled"
        } else {
            "not connected"
        };
        if !disabled
            && connection
                .as_ref()
                .is_some_and(|c| c.status() == ConnectionStatus::Connected)
        {
            status = "connected";
        } else if !disabled
            && connection
                .as_ref()
                .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
        {
            status = "needs-auth";
        } else if !disabled && failed_ago.is_some() {
            status = "failed";
        } else if !disabled && metadata.is_some() {
            status = "cached";
        }
        let mut entry = json!({
            "name": name,
            "status": status,
            "toolCount": tool_count,
            "failedAgo": failed_ago,
        });
        if disabled {
            entry["disabled"] = json!(true);
        }
        servers.push(entry);
    }
    drop(tool_metadata);

    let disabled_count = servers
        .iter()
        .filter(|s| s["disabled"].as_bool() == Some(true))
        .count();
    let enabled: Vec<&Value> = servers
        .iter()
        .filter(|s| s["disabled"].as_bool() != Some(true))
        .collect();
    let total_tools: u64 = enabled
        .iter()
        .map(|s| s["toolCount"].as_u64().unwrap_or(0))
        .sum();
    let connected_count = enabled
        .iter()
        .filter(|s| s["status"].as_str() == Some("connected"))
        .count();

    let mut text = format!(
        "MCP: {connected_count}/{} servers, {total_tools} tools",
        enabled.len()
    );
    if disabled_count > 0 {
        text.push_str(&format!(" ({disabled_count} disabled)"));
    }
    text.push_str("\n\n");
    for server in &servers {
        let name = server["name"].as_str().unwrap_or_default();
        let tool_count = server["toolCount"].as_u64().unwrap_or(0);
        match server["status"].as_str().unwrap_or("") {
            _ if server["disabled"].as_bool() == Some(true) => {
                text.push_str(&format!("⊘ {name} (disabled)\n"));
            }
            "connected" => text.push_str(&format!("✓ {name} ({tool_count} tools)\n")),
            "needs-auth" => text.push_str(&format!("⚠ {name} (needs auth)\n")),
            "cached" => text.push_str(&format!("○ {name} ({tool_count} tools, cached)\n")),
            "failed" => text.push_str(&format!(
                "✗ {name} (failed {}s ago)\n",
                server["failedAgo"].as_u64().unwrap_or(0)
            )),
            _ => text.push_str(&format!("○ {name} (not connected)\n")),
        }
    }
    if !servers.is_empty() {
        text.push_str(
            "\nmcp({ server: \"name\" }) to list tools, mcp({ search: \"...\" }) to search",
        );
    }

    json!({
        "content": [{ "type": "text", "text": text.trim() }],
        "details": {
            "mode": "status",
            "servers": servers,
            "totalTools": total_tools,
            "connectedCount": connected_count,
            "disabledCount": disabled_count,
        },
    })
}

/// `executeSearch` (proxy-modes.ts:458-594).
pub fn execute_search(
    state: &McpRuntime,
    query: &str,
    regex_mode: bool,
    server: Option<&str>,
    include_schemas: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Value {
    let show_schemas = include_schemas != Some(false);
    if let Some(server) = server {
        if state.config.is_server_disabled(server) {
            return disabled_result("search", server);
        }
    }

    let (config, metadata_snapshot) = search_state_snapshot(state);
    let search_state = SearchState {
        config: &config,
        tool_metadata: &metadata_snapshot,
    };

    #[derive(Clone)]
    struct MatchItem {
        server: String,
        tool: ToolMetadata,
        score: i64,
    }
    let matches: Vec<MatchItem>;
    if regex_mode {
        // JS `query.length` counts UTF-16 units.
        if query.encode_utf16().count() > MAX_REGEX_SEARCH_QUERY_LENGTH {
            return text_result(
                format!(
                    "Regex query is too long; maximum length is {MAX_REGEX_SEARCH_QUERY_LENGTH} characters."
                ),
                json!({
                    "mode": "search",
                    "error": "query_too_long",
                    "query": query,
                    "maxLength": MAX_REGEX_SEARCH_QUERY_LENGTH,
                }),
            );
        }
        // [VARIANT, requirements FR-P0-04]: Rust `regex` has no catastrophic
        // backtracking, so the upstream `recheck` safety grading collapses to
        // "compiles or not" — no `unsafe_pattern` kind.
        let pattern = match regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
        {
            Ok(pattern) => pattern,
            Err(_) => {
                return text_result(
                    format!("Invalid regex: {query}"),
                    json!({ "mode": "search", "error": "invalid_pattern", "query": query }),
                );
            }
        };
        let global_prefix = config.global_tool_prefix();
        let mut found = Vec::new();
        for (server_name, metadata) in &metadata_snapshot {
            let definition = config.mcp_servers.get(server_name);
            if definition.is_some_and(ServerEntry::is_disabled) {
                continue;
            }
            if let Some(server) = server {
                if server_name != server {
                    continue;
                }
            }
            for tool in metadata {
                let matched = pattern.is_match(&tool.name)
                    || pattern.is_match(&tool.description)
                    || resolve_search_keywords(
                        definition,
                        &tool.original_name,
                        server_name,
                        global_prefix,
                    )
                    .iter()
                    .any(|keyword| pattern.is_match(keyword));
                if matched {
                    found.push(MatchItem {
                        server: server_name.clone(),
                        tool: tool.clone(),
                        score: 0,
                    });
                }
            }
        }
        matches = found;
    } else if query.trim().is_empty() {
        let Some(server) = server else {
            return text_result(
                "Search query cannot be empty".to_string(),
                json!({ "mode": "search", "error": "empty_query" }),
            );
        };
        let mut found: Vec<MatchItem> = search_state
            .tool_metadata
            .iter()
            .find(|(name, _)| name == server)
            .map(|(_, metadata)| metadata)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|tool| MatchItem {
                server: server.to_string(),
                tool,
                score: 0,
            })
            .collect();
        found.sort_by(|a, b| a.tool.name.cmp(&b.tool.name));
        matches = found;
    } else {
        matches = rank_tool_matches(&search_state, query, server, true)
            .into_iter()
            .map(|m| MatchItem {
                server: m.server,
                tool: m.tool,
                score: m.score,
            })
            .collect();
    }

    let page = paginate(&matches, offset.unwrap_or(0), limit.unwrap_or(12));
    if page.total == 0 {
        let connecting: Vec<String> = match server {
            Some(server) => {
                if state.config.mcp_servers.contains_key(server)
                    && state.manager.is_connecting(server)
                {
                    vec![server.to_string()]
                } else {
                    Vec::new()
                }
            }
            None => {
                let mut names: Vec<String> = state
                    .config
                    .mcp_servers
                    .keys()
                    .filter(|name| {
                        !state.config.is_server_disabled(name) && state.manager.is_connecting(name)
                    })
                    .cloned()
                    .collect();
                names.sort();
                names
            }
        };
        let msg = match server {
            Some(server) => format!("No tools matching \"{query}\" in \"{server}\""),
            None => format!("No tools matching \"{query}\""),
        };
        let connecting_message = if connecting.len() == 1 {
            format!(
                " Server \"{}\" is still connecting; retry in a moment.",
                connecting[0]
            )
        } else if connecting.len() > 1 {
            format!(
                " Servers {} are still connecting; retry in a moment.",
                connecting
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            String::new()
        };
        let mut details = json!({
            "mode": "search",
            "matches": [],
            "count": 0,
            "hasMore": false,
            "nextOffset": null,
            "query": query,
        });
        if !connecting.is_empty() {
            details["connectingServers"] = json!(connecting);
        }
        return text_result(format!("{msg}{connecting_message}"), details);
    }

    let mut text = format!(
        "Found {} tool{} matching \"{query}\":\n\n",
        page.total,
        if page.total == 1 { "" } else { "s" }
    );
    for item in &page.items {
        if show_schemas {
            text.push_str(&format!("{}\n", item.tool.name));
            let description = if item.tool.description.is_empty() {
                "(no description)"
            } else {
                &item.tool.description
            };
            text.push_str(&format!("  {description}\n"));
            if let Some(schema) = &item.tool.input_schema {
                if item.tool.resource_uri.is_none() {
                    match render_ts_shape(schema) {
                        Some(shape) => {
                            text.push_str("\n  Shape:\n");
                            for line in shape.split('\n') {
                                text.push_str(&format!("    {line}\n"));
                            }
                        }
                        None => {
                            text.push_str("\n  Parameters:\n");
                            text.push_str(&format_schema(schema, "    "));
                            text.push('\n');
                        }
                    }
                }
            } else if item.tool.resource_uri.is_some() {
                text.push_str("  No parameters (resource tool).\n");
            }
            text.push('\n');
        } else {
            text.push_str(&format!("- {}", item.tool.name));
            if !item.tool.description.is_empty() {
                text.push_str(&format!(
                    " - {}",
                    truncate_at_word(&item.tool.description, 50)
                ));
            }
            text.push('\n');
        }
    }
    if page.has_more {
        text.push_str(&format!(
            "\n{} of {} — offset: {} for more\n",
            page.items.len(),
            page.total,
            page.next_offset.unwrap_or(0)
        ));
    }

    json!({
        "content": [{ "type": "text", "text": text.trim() }],
        "details": {
            "mode": "search",
            "matches": page.items.iter().map(|m| json!({
                "server": m.server,
                "tool": m.tool.name,
                "score": m.score,
            })).collect::<Vec<_>>(),
            "count": page.total,
            "hasMore": page.has_more,
            "nextOffset": page.next_offset,
            "query": query,
        },
    })
}

/// `executeDescribe` (proxy-modes.ts:406-456), minus the P1 approval marker.
pub fn execute_describe(state: &McpRuntime, tool_name: &str) -> Value {
    let tool_metadata = state
        .tool_metadata
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut server_name: Option<String> = None;
    let mut tool_meta: Option<ToolMetadata> = None;
    let mut disabled_match: Option<String> = None;
    for (server, metadata) in tool_metadata.iter() {
        let Some(found) = find_tool_by_name(metadata, tool_name) else {
            continue;
        };
        if state.config.is_server_disabled(server) {
            if disabled_match.is_none() {
                disabled_match = Some(server.clone());
            }
            continue;
        }
        server_name = Some(server.clone());
        tool_meta = Some(found.clone());
        break;
    }
    drop(tool_metadata);

    let (Some(server_name), Some(tool_meta)) = (server_name, tool_meta) else {
        if let Some(disabled) = disabled_match {
            return disabled_result("describe", &disabled);
        }
        let (config, snapshot) = search_state_snapshot(state);
        let search_state = SearchState {
            config: &config,
            tool_metadata: &snapshot,
        };
        let suggestions = rank_suggestions(&search_state, tool_name, 5);
        let suggestion_text = if suggestions.is_empty() {
            String::new()
        } else {
            format!(" Did you mean: {}", suggestions.join(", "))
        };
        return text_result(
            format!(
                "Tool \"{tool_name}\" not found. Use mcp({{ search: \"...\" }}) to search.{suggestion_text}"
            ),
            json!({
                "mode": "describe",
                "error": "tool_not_found",
                "requestedTool": tool_name,
                "suggestions": suggestions,
            }),
        );
    };

    let mut text = format!("{}\n", tool_meta.name);
    text.push_str(&format!("Server: {server_name}\n"));
    if let Some(uri) = &tool_meta.resource_uri {
        text.push_str(&format!("Type: Resource (reads from {uri})\n"));
    }
    text.push_str(&format!(
        "\n{}\n",
        if tool_meta.description.is_empty() {
            "(no description)"
        } else {
            &tool_meta.description
        }
    ));
    if tool_meta.input_schema.is_some() && tool_meta.resource_uri.is_none() {
        let schema = tool_meta.input_schema.clone().unwrap_or(Value::Null);
        match render_ts_shape(&schema) {
            Some(shape) => text.push_str(&format!("\nShape:\n{shape}")),
            None => text.push_str(&format!("\nParameters:\n{}", format_schema(&schema, "  "))),
        }
    } else if tool_meta.resource_uri.is_some() {
        text.push_str("\nNo parameters required (resource tool).");
    } else {
        text.push_str("\nNo parameters defined.");
    }

    json!({
        "content": [{ "type": "text", "text": text.trim() }],
        "details": {
            "mode": "describe",
            "tool": {
                "name": tool_meta.name,
                "originalName": tool_meta.original_name,
                "description": tool_meta.description,
                "resourceUri": tool_meta.resource_uri,
                "inputSchema": tool_meta.input_schema,
            },
            "server": server_name,
        },
    })
}

/// `executeList` (proxy-modes.ts:596-662).
pub fn execute_list(state: &McpRuntime, server: &str) -> Value {
    let Some(definition) = state.config.mcp_servers.get(server) else {
        return text_result(
            format!("Server \"{server}\" not found. Use mcp({{}}) to see available servers."),
            json!({ "mode": "list", "server": server, "tools": [], "count": 0, "error": "not_found" }),
        );
    };
    if definition.is_disabled() {
        return disabled_result("list", server);
    }
    let metadata = state
        .tool_metadata
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(server)
        .cloned();
    let tool_names: Vec<String> = metadata
        .as_ref()
        .map(|m| m.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();
    let connection = state.manager.get_connection(server);
    let instructions = state
        .server_instructions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(server)
        .cloned();
    let mut instructions_text = String::new();
    if let Some(instructions) = &instructions {
        let preview = truncate_at_word(instructions, INSTRUCTIONS_PREVIEW_LENGTH);
        instructions_text = format!("\n\nServer instructions:\n{preview}");
        if preview != *instructions {
            instructions_text.push_str(&format!(
                "\nUse mcp({{ instructions: \"{server}\" }}) for the full text."
            ));
        }
    }

    if tool_names.is_empty() {
        if connection
            .as_ref()
            .is_some_and(|c| c.status() == ConnectionStatus::Connected)
        {
            return text_result(
                format!("Server \"{server}\" has no tools.{instructions_text}"),
                json!({
                    "mode": "list", "server": server, "tools": [], "count": 0,
                    "hasInstructions": instructions.is_some(),
                }),
            );
        }
        if metadata.is_some() {
            return text_result(
                format!(
                    "Server \"{server}\" has no cached tools (not connected).{instructions_text}"
                ),
                json!({
                    "mode": "list", "server": server, "tools": [], "count": 0,
                    "cached": true, "hasInstructions": instructions.is_some(),
                }),
            );
        }
        return text_result(
            format!(
                "Server \"{server}\" is configured but not connected. Use mcp({{ connect: \"{server}\" }}) or /mcp reconnect {server} to retry.{instructions_text}"
            ),
            json!({
                "mode": "list", "server": server, "tools": [], "count": 0,
                "error": "not_connected", "hasInstructions": instructions.is_some(),
            }),
        );
    }

    let cached_note = if connection
        .as_ref()
        .is_some_and(|c| c.status() == ConnectionStatus::Connected)
    {
        ""
    } else {
        " (not connected, cached)"
    };
    let mut text = format!("{server} ({} tools{cached_note}):\n\n", tool_names.len());
    let descriptions: HashMap<String, String> = metadata
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|t| (t.name.clone(), t.description.clone()))
                .collect()
        })
        .unwrap_or_default();
    for tool in &tool_names {
        let truncated = truncate_at_word(descriptions.get(tool).map_or("", String::as_str), 50);
        text.push_str(&format!("- {tool}"));
        if !truncated.is_empty() {
            text.push_str(&format!(" - {truncated}"));
        }
        text.push('\n');
    }
    text.push_str(&instructions_text);

    json!({
        "content": [{ "type": "text", "text": text.trim() }],
        "details": {
            "mode": "list",
            "server": server,
            "tools": tool_names,
            "count": tool_names.len(),
            "hasInstructions": instructions.is_some(),
        },
    })
}

/// `executeInstructions` (proxy-modes.ts:664-694).
pub fn execute_instructions(state: &McpRuntime, server: &str) -> Value {
    if !state.config.mcp_servers.contains_key(server) {
        return text_result(
            format!("Server \"{server}\" not found. Use mcp({{}}) to see available servers."),
            json!({ "mode": "instructions", "server": server, "error": "not_found" }),
        );
    }
    if state.config.is_server_disabled(server) {
        return disabled_result("instructions", server);
    }
    let instructions = state
        .server_instructions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(server)
        .cloned();
    if let Some(instructions) = instructions {
        return json!({
            "content": [{ "type": "text", "text": format!("{server} instructions:\n\n{instructions}") }],
            "details": { "mode": "instructions", "server": server, "length": instructions.len() },
        });
    }
    let connected = state
        .manager
        .get_connection(server)
        .is_some_and(|c| c.status() == ConnectionStatus::Connected);
    if connected {
        return text_result(
            format!("Server \"{server}\" does not provide instructions."),
            json!({ "mode": "instructions", "server": server, "error": "no_instructions" }),
        );
    }
    text_result(
        format!(
            "No instructions cached for \"{server}\". Use mcp({{ connect: \"{server}\" }}) to connect and refresh."
        ),
        json!({ "mode": "instructions", "server": server, "error": "not_connected" }),
    )
}

/// `executeConnect` (proxy-modes.ts:696-766), P0 cut (no UI, no auto-auth).
pub async fn execute_connect(state: &McpRuntime, server_name: &str) -> Value {
    let Some(definition) = state.config.mcp_servers.get(server_name).cloned() else {
        return text_result(
            format!("Server \"{server_name}\" not found. Use mcp({{}}) to see available servers."),
            json!({ "mode": "connect", "error": "not_found", "server": server_name }),
        );
    };
    if definition.is_disabled() {
        return disabled_result("connect", server_name);
    }

    let current = state.manager.get_connection(server_name);
    let mut outcome = match current {
        Some(existing) if existing.status() == ConnectionStatus::Connected => {
            state
                .manager
                .reconnect(server_name, &definition, &existing)
                .await
        }
        _ => state.manager.connect(server_name, &definition).await,
    };
    // TE-D09 (proxy-modes.ts:716-734): one auto-auth attempt on
    // needs-auth, then a fresh connect.
    if let Ok(connection) = &outcome {
        if connection.status() == ConnectionStatus::NeedsAuth {
            match attempt_auto_auth(state, server_name).await {
                Err(message) => {
                    return text_result(
                        message.clone(),
                        json!({
                            "mode": "connect", "error": "auth_required",
                            "server": server_name, "message": message,
                        }),
                    );
                }
                Ok(true) => {
                    outcome = state.manager.connect(server_name, &definition).await;
                }
                Ok(false) => {}
            }
        }
    }
    match outcome {
        Ok(connection) => {
            if connection.status() == ConnectionStatus::NeedsAuth {
                let message = auth_required_message(&state.config, server_name);
                return text_result(
                    message.clone(),
                    json!({
                        "mode": "connect", "error": "auth_required",
                        "server": server_name, "message": message,
                    }),
                );
            }
            update_server_metadata(state, server_name);
            update_metadata_cache(state, server_name);
            notify_metadata_updated(state, server_name, "proxy-connect");
            mark_keep_alive_after_connect(state, server_name);
            state.failures.clear(server_name);
            execute_list(state, server_name)
        }
        Err(error) => {
            let message = error.to_string();
            state
                .failures
                .record(server_name, &message, state.owner_cancel.clone());
            text_result(
                format!("Failed to connect to \"{server_name}\": {message}"),
                json!({
                    "mode": "connect", "error": "connect_failed",
                    "server": server_name, "message": message,
                }),
            )
        }
    }
}

/// One `tools/call` (or `resources/read` for resource tools) against
/// `client` — the body that session recovery retries exactly once on the
/// fresh connection (`withSessionRecovery` callbacks, proxy-modes.ts:1122).
/// Shared with the direct-tools executor (direct-tools.ts:437-460).
pub async fn run_tool_call(
    client: &std::sync::Arc<crate::protocol::McpClient>,
    resource_uri: Option<&str>,
    original_name: &str,
    args: Value,
    request_timeout: std::time::Duration,
) -> Result<(Value, bool), crate::protocol::ProtocolError> {
    if let Some(uri) = resource_uri {
        client
            .read_resource(uri, request_timeout)
            .await
            .map(|value| (value, true))
    } else {
        client
            .call_tool(original_name, args, request_timeout)
            .await
            .map(|value| (value, false))
    }
}

/// `executeCall` (proxy-modes.ts:768-1253), P0 cut: no approval gate, no UI
/// sessions; guard-disabled content path. Session recovery (FR-P1-08) and
/// auto-auth (FR-P1-04) are wired in (TE-D09/TE-D11).
pub async fn execute_call(
    state: &McpRuntime,
    tool_name: &str,
    args: Option<serde_json::Map<String, Value>>,
    server_override: Option<&str>,
    native_tools: &[String],
) -> Value {
    let prefix_mode = state.config.global_tool_prefix();
    // proxy-modes.ts:783-799 — identity keys sit between `error` and
    // `message` (upstream `{ mode, error, ...identity, message }`).
    let disabled_call_result = |disabled_server: &str, metadata: Option<&ToolMetadata>| -> Value {
        let message = format!(
            "Server \"{disabled_server}\" is disabled. Run /mcp enable {disabled_server} and /reload to enable it."
        );
        let mut ordered = serde_json::Map::new();
        ordered.insert("mode".to_string(), json!("call"));
        ordered.insert("error".to_string(), json!("server_disabled"));
        match metadata {
            Some(meta) => {
                if let Some(uri) = &meta.resource_uri {
                    ordered.insert("server".to_string(), json!(disabled_server));
                    ordered.insert("resourceUri".to_string(), json!(uri));
                } else {
                    ordered.insert("server".to_string(), json!(disabled_server));
                    ordered.insert("tool".to_string(), json!(meta.original_name));
                }
            }
            None => {
                ordered.insert("server".to_string(), json!(disabled_server));
                ordered.insert("requestedTool".to_string(), json!(tool_name));
            }
        }
        ordered.insert("message".to_string(), json!(message));
        text_result(message, Value::Object(ordered))
    };

    let mut server_name: Option<String> = server_override.map(str::to_string);
    let mut tool_meta: Option<ToolMetadata> = None;

    if let Some(server) = &server_name {
        if !state.config.mcp_servers.contains_key(server) {
            return text_result(
                format!("Server \"{server}\" not found. Use mcp({{}}) to see available servers."),
                json!({
                    "mode": "call", "error": "server_not_found",
                    "server": server, "requestedTool": tool_name,
                }),
            );
        }
        tool_meta = state
            .tool_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server)
            .and_then(|m| find_tool_by_name(m, tool_name))
            .cloned();
        if state.config.is_server_disabled(server) {
            return disabled_call_result(server, tool_meta.as_ref());
        }
    } else {
        let mut disabled_match: Option<(String, ToolMetadata)> = None;
        let metadata = state
            .tool_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (server, tools) in metadata.iter() {
            let Some(found) = find_tool_by_name(tools, tool_name) else {
                continue;
            };
            if state.config.is_server_disabled(server) {
                if disabled_match.is_none() {
                    disabled_match = Some((server.clone(), found.clone()));
                }
                continue;
            }
            server_name = Some(server.clone());
            tool_meta = Some(found.clone());
            break;
        }
        drop(metadata);
        if tool_meta.is_none() {
            if let Some((server, meta)) = disabled_match {
                return disabled_call_result(&server, Some(&meta));
            }
        }
    }

    let mut auto_auth_attempted = false;
    if server_name.is_some() && tool_meta.is_none() {
        let server = server_name.clone().unwrap_or_default();
        if lazy_connect(state, &server).await {
            tool_meta = state
                .tool_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&server)
                .and_then(|m| find_tool_by_name(m, tool_name))
                .cloned();
        } else {
            // TE-D09 (proxy-modes.ts:832-869): needs-auth → one auto-auth
            // attempt → lazy_connect again; tool_not_found_after_reconnect
            // if the tool vanished with the fresh metadata.
            if state
                .manager
                .get_connection(&server)
                .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
                && !auto_auth_attempted
            {
                auto_auth_attempted = true;
                match attempt_auto_auth(state, &server).await {
                    Err(message) => {
                        return text_result(
                            message.clone(),
                            json!({
                                "mode": "call", "error": "auth_required",
                                "server": server, "requestedTool": tool_name, "message": message,
                            }),
                        );
                    }
                    Ok(true) => {
                        if lazy_connect(state, &server).await {
                            tool_meta = state
                                .tool_metadata
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&server)
                                .and_then(|m| find_tool_by_name(m, tool_name))
                                .cloned();
                            if tool_meta.is_none() {
                                let (config, snapshot) = search_state_snapshot(state);
                                let search_state = SearchState {
                                    config: &config,
                                    tool_metadata: &snapshot,
                                };
                                let suggestions = rank_suggestions(&search_state, tool_name, 5);
                                let suggestion_text = if suggestions.is_empty() {
                                    String::new()
                                } else {
                                    format!(" Did you mean: {}", suggestions.join(", "))
                                };
                                return text_result(
                                    format!(
                                        "Tool \"{tool_name}\" not found on \"{server}\" after reconnect.{suggestion_text}"
                                    ),
                                    json!({
                                        "mode": "call", "error": "tool_not_found_after_reconnect",
                                        "server": server, "requestedTool": tool_name,
                                        "suggestions": suggestions,
                                    }),
                                );
                            }
                        }
                    }
                    Ok(false) => {}
                }
            }
            if tool_meta.is_none()
                && state
                    .manager
                    .get_connection(&server)
                    .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
            {
                let message = auth_required_message(&state.config, &server);
                return text_result(
                    message.clone(),
                    json!({
                        "mode": "call", "error": "auth_required",
                        "server": server, "requestedTool": tool_name, "message": message,
                    }),
                );
            }
            if tool_meta.is_none() {
                if let Some(failed_ago) = state.failures.failure_age_seconds(&server) {
                    return text_result(
                        format!(
                            "Server \"{server}\" not available (last failed {failed_ago}s ago)"
                        ),
                        json!({
                            "mode": "call", "error": "server_backoff",
                            "server": server, "requestedTool": tool_name,
                        }),
                    );
                }
            }
        }
    }

    let mut prefix_matched_server: Option<String> = None;
    if server_name.is_none()
        && tool_meta.is_none()
        && prefix_mode != crate::metadata::ToolPrefix::None
    {
        let mut candidates: Vec<(String, String)> = state
            .config
            .mcp_servers
            .iter()
            .filter(|(_, d)| !d.is_disabled())
            .map(|(name, _)| (name.clone(), get_server_prefix(name, prefix_mode)))
            .filter(|(_, prefix)| {
                !prefix.is_empty() && tool_name.starts_with(&format!("{prefix}_"))
            })
            .collect();
        candidates.sort_by_key(|(_, prefix)| std::cmp::Reverse(prefix.len()));

        for (configured_server, _) in candidates {
            let existing = state.manager.get_connection(&configured_server);
            let failed_ago = state.failures.failure_age_seconds(&configured_server);
            if failed_ago.is_some()
                && existing
                    .as_ref()
                    .is_none_or(|c| c.status() != ConnectionStatus::NeedsAuth)
            {
                continue;
            }
            let mut connected = lazy_connect(state, &configured_server).await;
            // TE-D09 (proxy-modes.ts:896-920)
            if !connected
                && !auto_auth_attempted
                && state
                    .manager
                    .get_connection(&configured_server)
                    .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
            {
                auto_auth_attempted = true;
                match attempt_auto_auth(state, &configured_server).await {
                    Err(message) => {
                        return text_result(
                            message.clone(),
                            json!({
                                "mode": "call", "error": "auth_required",
                                "server": configured_server, "requestedTool": tool_name,
                                "message": message,
                            }),
                        );
                    }
                    Ok(true) => {
                        connected = lazy_connect(state, &configured_server).await;
                    }
                    Ok(false) => {}
                }
            }
            if !connected {
                if state
                    .manager
                    .get_connection(&configured_server)
                    .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
                {
                    let message = auth_required_message(&state.config, &configured_server);
                    return text_result(
                        message.clone(),
                        json!({
                            "mode": "call", "error": "auth_required",
                            "server": configured_server, "requestedTool": tool_name,
                            "message": message,
                        }),
                    );
                }
                continue;
            }
            if prefix_matched_server.is_none() {
                prefix_matched_server = Some(configured_server.clone());
            }
            tool_meta = state
                .tool_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&configured_server)
                .and_then(|m| find_tool_by_name(m, tool_name))
                .cloned();
            if tool_meta.is_some() {
                server_name = Some(configured_server);
                break;
            }
        }
    }

    let (Some(server_name), Some(tool_meta)) = (server_name.clone(), tool_meta.clone()) else {
        if server_override.is_none() && native_tools.iter().any(|t| t == tool_name && t != "mcp") {
            return text_result(
                format!(
                    "\"{tool_name}\" is a native Pi tool. Call {tool_name} directly instead of using mcp({{ tool: \"{tool_name}\" }})."
                ),
                json!({ "mode": "call", "error": "native_tool", "requestedTool": tool_name }),
            );
        }
        let hint_server = server_name.clone().or(prefix_matched_server);
        let available: Vec<String> = hint_server
            .as_ref()
            .and_then(|server| {
                state
                    .tool_metadata
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(server)
                    .map(|m| m.iter().map(|t| t.name.clone()).collect())
            })
            .unwrap_or_default();
        let mut msg = format!("Tool \"{tool_name}\" not found.");
        if !available.is_empty() {
            msg.push_str(&format!(
                " Server \"{}\" has: {}",
                hint_server.clone().unwrap_or_default(),
                available.join(", ")
            ));
        } else {
            msg.push_str(" Use mcp({ search: \"...\" }) to search.");
        }
        let (config, snapshot) = search_state_snapshot(state);
        let search_state = SearchState {
            config: &config,
            tool_metadata: &snapshot,
        };
        let suggestions = rank_suggestions(&search_state, tool_name, 5);
        if !suggestions.is_empty() {
            msg.push_str(&format!(" Did you mean: {}", suggestions.join(", ")));
        }
        return text_result(
            msg,
            json!({
                "mode": "call", "error": "tool_not_found",
                "requestedTool": tool_name, "hintServer": hint_server, "suggestions": suggestions,
            }),
        );
    };

    let call_identity = match &tool_meta.resource_uri {
        Some(uri) => json!({ "server": server_name, "resourceUri": uri }),
        None => json!({ "server": server_name, "tool": tool_meta.original_name }),
    };

    let mut connection = state.manager.get_connection(&server_name);
    if connection
        .as_ref()
        .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
    {
        // TE-D09 (proxy-modes.ts:954-977): one auto-auth attempt before
        // surfacing the needs-auth guidance.
        if !auto_auth_attempted {
            auto_auth_attempted = true;
            match attempt_auto_auth(state, &server_name).await {
                Err(message) => {
                    let mut details = json!({ "mode": "call", "error": "auth_required" });
                    merge_objects(&mut details, &call_identity);
                    details["message"] = json!(message);
                    return text_result(message, details);
                }
                Ok(true) => {
                    connection = state.manager.get_connection(&server_name);
                }
                Ok(false) => {}
            }
        }

        if connection
            .as_ref()
            .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
        {
            let message = auth_required_message(&state.config, &server_name);
            let mut details = json!({ "mode": "call", "error": "auth_required" });
            merge_objects(&mut details, &call_identity);
            details["message"] = json!(message);
            details["autoAuthAttempted"] = json!(auto_auth_attempted);
            return text_result(message, details);
        }
    }
    if connection
        .as_ref()
        .is_none_or(|c| c.status() != ConnectionStatus::Connected)
    {
        if let Some(failed_ago) = state.failures.failure_age_seconds(&server_name) {
            let mut details = json!({ "mode": "call", "error": "server_backoff" });
            merge_objects(&mut details, &call_identity);
            return text_result(
                format!("Server \"{server_name}\" not available (last failed {failed_ago}s ago)"),
                details,
            );
        }
        let Some(definition) = state.config.mcp_servers.get(&server_name).cloned() else {
            let mut details = json!({ "mode": "call", "error": "server_not_connected" });
            merge_objects(&mut details, &call_identity);
            return text_result(format!("Server \"{server_name}\" not connected"), details);
        };
        match state.manager.connect(&server_name, &definition).await {
            Ok(mut new_connection) => {
                // TE-D09 (proxy-modes.ts:995-1023): needs-auth right after
                // a connect → one auto-auth attempt → connect again.
                if new_connection.status() == ConnectionStatus::NeedsAuth {
                    if !auto_auth_attempted {
                        auto_auth_attempted = true;
                        match attempt_auto_auth(state, &server_name).await {
                            Err(message) => {
                                let mut details =
                                    json!({ "mode": "call", "error": "auth_required" });
                                merge_objects(&mut details, &call_identity);
                                details["message"] = json!(message);
                                return text_result(message, details);
                            }
                            Ok(true) => {
                                new_connection = state
                                    .manager
                                    .connect(&server_name, &definition)
                                    .await
                                    .unwrap_or(new_connection);
                            }
                            Ok(false) => {}
                        }
                    }
                    if new_connection.status() == ConnectionStatus::NeedsAuth {
                        let message = auth_required_message(&state.config, &server_name);
                        let mut details = json!({ "mode": "call", "error": "auth_required" });
                        merge_objects(&mut details, &call_identity);
                        details["message"] = json!(message);
                        return text_result(message, details);
                    }
                }
                state.failures.clear(&server_name);
                update_server_metadata(state, &server_name);
                update_metadata_cache(state, &server_name);
                notify_metadata_updated(state, &server_name, "proxy-call-reconnect");
                mark_keep_alive_after_connect(state, &server_name);
                connection = Some(new_connection);
            }
            Err(error) => {
                let message = error.to_string();
                state
                    .failures
                    .record(&server_name, &message, state.owner_cancel.clone());
                let mut details = json!({ "mode": "call", "error": "connect_failed" });
                merge_objects(&mut details, &call_identity);
                details["message"] = json!(message);
                return text_result(
                    format!("Failed to connect to \"{server_name}\": {message}"),
                    details,
                );
            }
        }
    }

    let request_timeout = state
        .config
        .mcp_servers
        .get(&server_name)
        .map(|d| state.manager.request_timeout(d))
        .unwrap_or(crate::protocol::DEFAULT_REQUEST_TIMEOUT);

    state.manager.touch(&server_name);
    state.manager.increment_in_flight(&server_name);

    // TE-D11: session recovery — if the tool call fails with a stale
    // session (HTTP 404 with session id), reconnect once and retry.
    let connection_arc = connection.clone();
    let result: Result<(Value, bool), String> = async {
        let Some(client) = connection_arc.as_ref().and_then(|c| c.client.clone()) else {
            return Err("server not connected".to_string());
        };
        let had_session_id = client.session_id().is_some();
        let call_result = run_tool_call(
            &client,
            tool_meta.resource_uri.as_deref(),
            &tool_meta.original_name,
            Value::Object(args.clone().unwrap_or_default()),
            request_timeout,
        )
        .await;

        match call_result {
            Ok(ok) => Ok(ok),
            Err(error) => {
                // Check if this is a terminated session (FR-P1-08).
                if session_recovery::is_terminated_session(&error, had_session_id) {
                    tracing::debug!(server = %server_name, "MCP session expired; reconnecting");
                    let Some(definition) = state.config.mcp_servers.get(&server_name) else {
                        return Err(error.to_string());
                    };
                    let Some(stale) = connection_arc.as_ref() else {
                        return Err(error.to_string());
                    };
                    match state
                        .manager
                        .reconnect(&server_name, definition, stale)
                        .await
                    {
                        Ok(fresh) if fresh.status() == ConnectionStatus::Connected => {
                            let Some(fresh_client) = &fresh.client else {
                                return Err(error.to_string());
                            };
                            // Retry once on the fresh connection.
                            run_tool_call(
                                fresh_client,
                                tool_meta.resource_uri.as_deref(),
                                &tool_meta.original_name,
                                Value::Object(args.clone().unwrap_or_default()),
                                request_timeout,
                            )
                            .await
                            .map_err(|e| e.to_string())
                        }
                        Ok(fresh) if fresh.status() == ConnectionStatus::NeedsAuth => {
                            // TE-D09 + TE-D11 (proxy-modes.ts:1078-1091
                            // `recoverAuthConnection`): the fresh session
                            // needs auth — one auto-auth attempt (the same
                            // per-call flag as the connect paths), then
                            // retry once on the re-authed connection;
                            // otherwise surface the recovery auth message.
                            let message = if !auto_auth_attempted {
                                auto_auth_attempted = true;
                                match attempt_auto_auth(state, &server_name).await {
                                    Err(message) => message,
                                    Ok(true) => {
                                        let reauthed = state.manager.get_connection(&server_name);
                                        match reauthed.map(|c| (c.status(), c.client.clone())) {
                                            Some((ConnectionStatus::Connected, Some(client))) => {
                                                return run_tool_call(
                                                    &client,
                                                    tool_meta.resource_uri.as_deref(),
                                                    &tool_meta.original_name,
                                                    Value::Object(args.clone().unwrap_or_default()),
                                                    request_timeout,
                                                )
                                                .await
                                                .map_err(|e| e.to_string());
                                            }
                                            _ => session_recovery::auth_required_error_message(
                                                &server_name,
                                            ),
                                        }
                                    }
                                    Ok(false) => {
                                        session_recovery::auth_required_error_message(&server_name)
                                    }
                                }
                            } else {
                                session_recovery::auth_required_error_message(&server_name)
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
    state.manager.decrement_in_flight(&server_name);
    state.manager.touch(&server_name);

    let guard_options = crate::guard::resolve_guard_options(state.config.settings.as_ref());
    match result {
        Ok((value, is_resource)) => {
            let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
            if is_resource {
                let content = value
                    .get("contents")
                    .and_then(Value::as_array)
                    .map(|a| transform_mcp_resource_contents(a))
                    .unwrap_or_default();
                let content = if content.is_empty() {
                    vec![json!({ "type": "text", "text": "(empty resource)" })]
                } else {
                    content
                };
                let guarded = crate::guard::guard_mcp_output(content, &guard_options);
                let mut details = json!({ "mode": "call" });
                merge_objects(&mut details, &call_identity);
                merge_objects(&mut details, &crate::guard::guarded_mcp_details(&guarded));
                return json!({ "content": guarded.content, "details": details });
            }
            if is_error {
                let content = value
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| transform_mcp_content(a))
                    .unwrap_or_default();
                let content = if content.is_empty() {
                    vec![json!({ "type": "text", "text": "(empty result)" })]
                } else {
                    content
                };
                let schema_text = tool_meta
                    .input_schema
                    .as_ref()
                    .map(|s| format!("\n\nExpected parameters:\n{}", format_schema(s, "  ")))
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
                        raw_mcp_result: Some(value),
                        ..guard_options.clone()
                    },
                );
                let mut details = json!({ "mode": "call", "error": "tool_error" });
                merge_objects(&mut details, &call_identity);
                merge_objects(&mut details, &crate::guard::guarded_mcp_details(&guarded));
                return json!({ "content": guarded.content, "details": details });
            }
            let content = resolve_mcp_result_content(&value);
            let content = if content.is_empty() {
                vec![json!({ "type": "text", "text": "(empty result)" })]
            } else {
                content
            };
            let guarded = crate::guard::guard_mcp_output(
                content,
                &crate::guard::GuardOptions {
                    raw_mcp_result: Some(value),
                    ..guard_options.clone()
                },
            );
            let mut details = json!({ "mode": "call" });
            merge_objects(&mut details, &call_identity);
            merge_objects(&mut details, &crate::guard::guarded_mcp_details(&guarded));
            json!({ "content": guarded.content, "details": details })
        }
        Err(message) => {
            let schema_text = tool_meta
                .input_schema
                .as_ref()
                .map(|s| format!("\n\nExpected parameters:\n{}", format_schema(s, "  ")))
                .unwrap_or_default();
            let guarded = crate::guard::guard_mcp_output(
                vec![json!({ "type": "text", "text": message.clone() })],
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
            let mut details = json!({ "mode": "call", "error": "call_failed" });
            merge_objects(&mut details, &call_identity);
            details["message"] = if guarded.output_guard.is_some() {
                json!("output truncated; see outputGuard.fullOutputPath")
            } else {
                json!(message)
            };
            merge_objects(&mut details, &crate::guard::guarded_mcp_details(&guarded));
            json!({ "content": guarded.content, "details": details })
        }
    }
}

/// Merge `source` object's keys into `target` (upstream object spread).
fn merge_objects(target: &mut Value, source: &Value) {
    if let (Some(t), Some(s)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in s {
            t.insert(key.clone(), value.clone());
        }
    }
}

// ============================================================================
// Dispatch (index.ts registerProxyTool execute + init gating)
// ============================================================================

/// Initialization state machine behind the 30s gate (index.ts:72-75,
/// 758-783).
#[derive(Clone)]
enum InitState {
    NotStarted,
    Initializing(Shared<BoxFuture<'static, Result<Arc<McpRuntime>, Arc<String>>>>),
    Ready(Arc<McpRuntime>),
    /// Initialization returned an error (currently unreachable:
    /// `initialize_mcp` degrades per-server instead of failing; kept so the
    /// `init_failed` gate arm stays wired for future hard-failure paths).
    #[allow(dead_code)]
    Failed(Arc<String>),
}

/// Surface-sync hooks fired by the dispatcher (the directTools surface
/// lives in lib.rs behind host calls).
#[derive(Default)]
pub struct DispatcherHooks {
    /// Fires once when init transitions to Ready (index.ts:302-326).
    pub on_ready: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Fires after `mcp({ connect })` (index.ts:822-824 — unconditional
    /// syncToolSurface, freeze-exempt).
    pub on_connect_sync: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// The proxy tool dispatcher: owns the runtime init state.
pub struct ProxyDispatcher {
    state: Mutex<InitState>,
    hooks: Mutex<DispatcherHooks>,
    /// Gate wait bound; `INIT_WAIT_TIMEOUT` in production, injectable so the
    /// `init_timeout` arm is testable without 30s wall-clock waits.
    init_wait_timeout: Mutex<std::time::Duration>,
}

impl Default for ProxyDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyDispatcher {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InitState::NotStarted),
            hooks: Mutex::new(DispatcherHooks::default()),
            init_wait_timeout: Mutex::new(INIT_WAIT_TIMEOUT),
        }
    }

    /// Test seam (design §5.2): shrink the gate wait bound. The reported
    /// `timeoutMs` stays the production constant (index.ts:38).
    pub fn set_init_wait_timeout(&self, timeout: std::time::Duration) {
        *self
            .init_wait_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = timeout;
    }

    /// Test seam (design §5.2): install a caller-built init future so the
    /// `init_timeout` / `init_failed` gate arms can be exercised by direct
    /// injection (a never-resolving or failing future) instead of real
    /// 30-second initialization paths.
    pub fn start_init_with(
        self: &Arc<Self>,
        future: Shared<BoxFuture<'static, Result<Arc<McpRuntime>, Arc<String>>>>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            InitState::Ready(_) | InitState::Initializing(_) => return,
            _ => {}
        }
        *state = InitState::Initializing(future);
    }

    /// Peek at the ready runtime without waiting (surface sync at install /
    /// metadata-update time).
    pub fn try_runtime(&self) -> Option<Arc<McpRuntime>> {
        match &*self.state.lock().unwrap_or_else(|e| e.into_inner()) {
            InitState::Ready(runtime) => Some(runtime.clone()),
            _ => None,
        }
    }

    pub fn set_hooks(&self, hooks: DispatcherHooks) {
        *self.hooks.lock().unwrap_or_else(|e| e.into_inner()) = hooks;
    }

    fn fire_on_ready(&self) {
        let hook = self
            .hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_ready
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn fire_on_connect_sync(&self) {
        let hook = self
            .hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_connect_sync
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Start initialization (session_start / load-time prewarm).
    ///
    /// Rust futures are lazy: storing the shared future alone would never
    /// poll it, so prewarm would silently do nothing and the first dispatch
    /// would drive all of init on the host's calling thread. This spawns a
    /// background driver task (upstream `startInitialization` fires
    /// immediately via `setImmediate` and runs its post-init sync on
    /// completion) that polls the future to completion, publishes the Ready
    /// runtime and fires `on_ready`; `current`/`current_direct`/`shutdown`
    /// awaiters race it against their own shared clone (the transition guard
    /// prevents double-firing). Outside a tokio context (non-runtime
    /// callers) the future stays lazy — the test-injection path
    /// (`start_init_with`) is unaffected.
    pub fn start_init(self: &Arc<Self>, cwd: PathBuf, config_path: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            InitState::Ready(_) | InitState::Initializing(_) => return,
            _ => {}
        }
        let future = async move { initialize_mcp(&cwd, config_path.as_deref(), None).await }
            .map(Ok)
            .boxed()
            .shared();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let dispatcher = Arc::clone(self);
            let driver = future.clone();
            handle.spawn(async move {
                if let Ok(runtime) = driver.await {
                    dispatcher.finish_init(runtime);
                }
            });
        }
        *state = InitState::Initializing(future);
    }

    /// Driver-task completion: publish the ready runtime and fire `on_ready`
    /// (guarded by the Initializing -> Ready transition so concurrent gate
    /// awaiters do not double-fire).
    fn finish_init(self: &Arc<Self>, runtime: Arc<McpRuntime>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(&*state, InitState::Initializing(_)) {
            *state = InitState::Ready(runtime);
            drop(state);
            self.fire_on_ready();
        }
    }

    /// Whether a startup server (eager/keep-alive) exists — the load-time
    /// prewarm condition (index.ts:352-357 `startLoadTimeInitialization`).
    pub fn has_startup_server(config: &McpConfig) -> bool {
        config.mcp_servers.values().any(|d| {
            !d.is_disabled()
                && matches!(
                    LifecycleMode::of(d),
                    LifecycleMode::Eager | LifecycleMode::KeepAlive
                )
        })
    }

    /// Owning wrapper for `shutdown`, for `block_on` call sites.
    pub async fn shutdown_owned(self: Arc<Self>) {
        self.shutdown().await;
    }

    /// Session restart: drop the old runtime (its CancellationToken cancels
    /// tasks; `graceful_shutdown` reaps children) and reset the gate.
    pub async fn shutdown(&self) {
        let old = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *state, InitState::NotStarted)
        };
        match old {
            InitState::Ready(runtime) => {
                runtime.owner_cancel.cancel();
                flush_metadata_cache(&runtime);
                runtime.lifecycle.graceful_shutdown().await;
            }
            InitState::Initializing(future) => {
                if let Ok(runtime) = future.await {
                    runtime.owner_cancel.cancel();
                    flush_metadata_cache(&runtime);
                    runtime.lifecycle.graceful_shutdown().await;
                }
            }
            _ => {}
        }
    }

    async fn current(&self) -> Result<Arc<McpRuntime>, GateError> {
        let shared = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*state {
                InitState::Ready(runtime) => return Ok(runtime.clone()),
                InitState::Failed(message) => return Err(GateError::Failed(message.to_string())),
                InitState::NotStarted => return Err(GateError::NotInitialized),
                InitState::Initializing(shared) => shared.clone(),
            }
        };
        let bound = *self
            .init_wait_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match tokio::time::timeout(bound, shared).await {
            Ok(Ok(runtime)) => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                // Guarded like `current_direct`: the background driver may
                // have published Ready already (and a shutdown may have reset
                // the gate) while this awaiter was parked on its shared clone.
                if matches!(&*state, InitState::Initializing(_)) {
                    *state = InitState::Ready(runtime.clone());
                    drop(state);
                    self.fire_on_ready();
                }
                Ok(runtime)
            }
            Ok(Err(message)) => Err(GateError::Failed(message.to_string())),
            Err(_) => Err(GateError::Timeout),
        }
    }

    /// The direct-tool executor's gate (direct-tools.ts:310-326): waits for
    /// the in-flight init WITHOUT the proxy tool's 30s timeout, and maps
    /// failure/absence to `init_failed`/`not_initialized` results. The lack
    /// of a bound is upstream parity; since the B-1 fix the future is driven
    /// by a background task, so this wait is bounded by the time init itself
    /// takes to complete.
    pub async fn current_direct(&self) -> Result<Arc<McpRuntime>, Value> {
        let shared = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*state {
                InitState::Ready(runtime) => return Ok(runtime.clone()),
                InitState::Failed(message) => {
                    return Err(json!({
                        "content": [{ "type": "text", "text": format!("MCP initialization failed: {message}") }],
                        "details": { "error": "init_failed", "message": message.to_string() },
                    }));
                }
                InitState::NotStarted => {
                    return Err(json!({
                        "content": [{ "type": "text", "text": "MCP not initialized" }],
                        "details": { "error": "not_initialized" },
                    }));
                }
                InitState::Initializing(shared) => shared.clone(),
            }
        };
        match shared.await {
            Ok(runtime) => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(&*state, InitState::Initializing(_)) {
                    *state = InitState::Ready(runtime.clone());
                    drop(state);
                    self.fire_on_ready();
                }
                Ok(runtime)
            }
            Err(message) => Err(json!({
                "content": [{ "type": "text", "text": format!("MCP initialization failed: {message}") }],
                "details": { "error": "init_failed", "message": message.to_string() },
            })),
        }
    }

    /// The proxy tool's execute body (index.ts:720-839).
    pub async fn execute(self: &Arc<Self>, params: &Value, native_tools: &[String]) -> Value {
        // args parsing (index.ts:735-756) — upstream throws; the ABI has no
        // throw channel, so the same message rides a normal result (TE-D04).
        let mut parsed_args: Option<serde_json::Map<String, Value>> = None;
        if let Some(args) = params.get("args") {
            if args.as_str() != Some("") {
                let parsed = if let Some(text) = args.as_str() {
                    match serde_json::from_str::<Value>(text) {
                        Ok(value) => value,
                        Err(error) => {
                            let message = format!("Invalid args JSON: {error}");
                            return text_result(
                                message.clone(),
                                json!({ "error": "invalid_args", "message": message }),
                            );
                        }
                    }
                } else {
                    args.clone()
                };
                match parsed.as_object() {
                    Some(map) => parsed_args = Some(map.clone()),
                    None => {
                        let got = if parsed.is_array() {
                            "array"
                        } else if parsed.is_null() {
                            "null"
                        } else if parsed.is_boolean() {
                            "boolean"
                        } else if parsed.is_number() {
                            "number"
                        } else {
                            "string"
                        };
                        let message = format!("Invalid args: expected a JSON object, got {got}");
                        return text_result(
                            message.clone(),
                            json!({ "error": "invalid_args", "message": message }),
                        );
                    }
                }
            }
        }

        let runtime = match self.current().await {
            Ok(runtime) => runtime,
            Err(GateError::Timeout) => {
                return text_result(
                    "MCP initialization is still in progress. Try again shortly.".to_string(),
                    json!({ "error": "init_timeout", "timeoutMs": 30000 }),
                );
            }
            Err(GateError::Failed(message)) => {
                return text_result(
                    format!("MCP initialization failed: {message}"),
                    json!({ "error": "init_failed", "message": message }),
                );
            }
            Err(GateError::NotInitialized) => {
                return text_result(
                    "MCP not initialized".to_string(),
                    json!({ "error": "not_initialized" }),
                );
            }
        };

        let action = params.get("action").and_then(Value::as_str);
        match action {
            Some("ui-messages") => {
                return text_result(
                    "MCP UI sessions are not available in this build (P2 scope).".to_string(),
                    json!({ "mode": "ui-messages", "error": "not_supported" }),
                );
            }
            Some("auth-start") => {
                let Some(server) = params.get("server").and_then(Value::as_str) else {
                    return text_result(
                        "auth-start requires `server`. Example: mcp({ action: \"auth-start\", server: \"linear-server\" })".to_string(),
                        json!({ "mode": "auth-start", "error": "missing_server" }),
                    );
                };
                return text_result(
                    auth_required_message(&runtime.config, server),
                    json!({ "mode": "auth-start", "error": "not_supported", "server": server }),
                );
            }
            Some("auth-complete") => {
                return text_result(
                    "OAuth is not available in this build (P1 scope).".to_string(),
                    json!({ "mode": "auth-complete", "error": "not_supported" }),
                );
            }
            _ => {}
        }

        if let Some(tool) = params.get("tool").and_then(Value::as_str) {
            let server = params.get("server").and_then(Value::as_str);
            return execute_call(&runtime, tool, parsed_args, server, native_tools).await;
        }
        if let Some(connect) = params.get("connect").and_then(Value::as_str) {
            let result = execute_connect(&runtime, connect).await;
            self.fire_on_connect_sync();
            return result;
        }
        if let Some(describe) = params.get("describe").and_then(Value::as_str) {
            return execute_describe(&runtime, describe);
        }
        if let Some(instructions) = params.get("instructions").and_then(Value::as_str) {
            return execute_instructions(&runtime, instructions);
        }
        if let Some(search) = params.get("search").and_then(Value::as_str) {
            return execute_search(
                &runtime,
                search,
                params.get("regex").and_then(Value::as_bool) == Some(true),
                params.get("server").and_then(Value::as_str),
                params.get("includeSchemas").and_then(Value::as_bool),
                params.get("limit").and_then(Value::as_i64),
                params.get("offset").and_then(Value::as_i64),
            );
        }
        if let Some(server) = params.get("server").and_then(Value::as_str) {
            return execute_list(&runtime, server);
        }
        execute_status(&runtime)
    }
}

enum GateError {
    Timeout,
    Failed(String),
    NotInitialized,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(settings: Value) -> McpConfig {
        McpConfig {
            settings: Some(settings.as_object().cloned().unwrap_or_default()),
            ..Default::default()
        }
    }

    /// `getAuthFailedMessage` (proxy-modes.ts:56-62): default guidance vs
    /// custom `authRequiredMessage` template.
    #[test]
    fn auth_failed_message_default_and_custom() {
        let default = auth_failed_message(&config(json!({})), "demo", "token endpoint 500");
        assert!(
            default.starts_with("OAuth authentication failed for \"demo\": token endpoint 500."),
            "{default}"
        );
        assert!(default.contains("auth-start"), "{default}");
        assert!(default.contains("/mcp-auth demo"), "{default}");

        let custom = auth_failed_message(
            &config(json!({ "authRequiredMessage": "Reconnect ${server} from the host app." })),
            "demo",
            "token endpoint 500",
        );
        assert_eq!(
            custom,
            "OAuth authentication failed for \"demo\": token endpoint 500. \
             Reconnect demo from the host app."
        );
    }

    /// `authRequiredMessage` also applies to the headless auto-auth guard
    /// (proxy-modes.ts:121-129 via getAuthRequiredMessage).
    #[test]
    fn auth_required_message_custom_template() {
        let custom = auth_required_message(
            &config(json!({ "authRequiredMessage": "Reconnect ${server} from the host app." })),
            "demo",
        );
        assert_eq!(custom, "Reconnect demo from the host app.");
    }
}
