//! Persistent MCP metadata cache (`mcp-cache.json`) — FR-P0-10.
//!
//! Port of `metadata-cache.ts` @ pi-mcp-adapter v2.24.0 (3d953f90):
//! `stableStringify`, `computeServerHash`, `isServerCacheValid`,
//! `loadMetadataCache` / `saveMetadataCache` (read-merge + tmp+rename atomic
//! write), and the (de)serializers `serializeTools` / `serializeResources` /
//! `serializePrompts` / `reconstructToolMetadata` /
//! `reconstructPromptMetadata`.
//!
//! Cache location (ADR-0001): `<agent dir>/mcp-cache.json` where the agent
//! dir resolves via `RPI_CODING_AGENT_DIR` → `~/.rpi/agent`.
//!
//! Critical parity note on `stableStringify` (metadata-cache.ts:311-322):
//! upstream does NOT skip absent fields — the identity object in
//! `computeServerHash` always carries all 14 keys, and `JSON.stringify(
//! undefined)` collapses to the literal string `undefined`, so a missing
//! field hashes as `"key":"undefined"`. `null` (present in the JSON) hashes
//! as `"key":null`. Keep the two apart.
//!
//! P0 scope cuts: `_meta`-derived cache fields (`uiResourceUri` /
//! `uiVisibility` / `uiStreamMode`) are MCP UI (P2) and are never populated
//! by `serialize_tools`; the struct fields exist so upstream-written cache
//! files round-trip byte-identically.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;

use crate::error::AdapterError;
use crate::metadata::{
    format_prompt_command_name, format_tool_name, is_tool_allowed, resolve_tool_prefix,
    resource_name_to_tool_name, McpResource, McpTool, ServerEntry, ToolMetadata, ToolPrefix,
};

/// `CACHE_VERSION` (metadata-cache.ts:33).
pub const CACHE_VERSION: u64 = 1;
/// `CACHE_MAX_AGE_MS` (metadata-cache.ts:34): seven days.
pub const CACHE_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// `CachedTool` (types.ts:592-599).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_resource_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_visibility: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_stream_mode: Option<Value>,
}

/// `CachedResource` (types.ts:601-605).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedResource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `CachedPrompt` (types.ts:607-612).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedPrompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<CachedPromptArgument>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedPromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// `ServerCacheEntry` (types.ts:614-621).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCacheEntry {
    pub config_hash: String,
    #[serde(default)]
    pub tools: Vec<CachedTool>,
    #[serde(default)]
    pub resources: Vec<CachedResource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<CachedPrompt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Unix epoch milliseconds (JS `Date.now()`).
    pub cached_at: u64,
}

/// `MetadataCache` (types.ts:623-626).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetadataCache {
    pub version: u64,
    pub servers: IndexMap<String, ServerCacheEntry>,
}

/// `getMetadataCachePath` (metadata-cache.ts:38-40).
pub fn get_metadata_cache_path() -> PathBuf {
    crate::config::get_agent_dir().join("mcp-cache.json")
}

/// `loadMetadataCache` (metadata-cache.ts:42-54): unreadable/malformed files,
/// foreign `version` and non-object `servers` all yield `None`. Individual
/// entries that fail typed deserialization are dropped (upstream keeps them
/// raw; they can never pass `isServerCacheValid`, so the observable behavior
/// is identical).
pub fn load_metadata_cache(path: &Path) -> Option<MetadataCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let obj = value.as_object()?;
    if obj.get("version").and_then(Value::as_u64) != Some(CACHE_VERSION) {
        return None;
    }
    let servers = obj.get("servers")?.as_object()?;
    let mut parsed = IndexMap::new();
    for (name, entry) in servers {
        if let Ok(entry) = serde_json::from_value::<ServerCacheEntry>(entry.clone()) {
            parsed.insert(name.clone(), entry);
        }
    }
    Some(MetadataCache {
        version: CACHE_VERSION,
        servers: parsed,
    })
}

/// `saveMetadataCache` (metadata-cache.ts:56-79): merge into the existing
/// file's server map, then write `{path}.{pid}.tmp` and rename over the
/// target (atomic replace).
pub fn save_metadata_cache(path: &Path, cache: &MetadataCache) -> Result<(), AdapterError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let mut merged_servers = IndexMap::new();
    if let Ok(raw) = std::fs::read_to_string(path) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            let obj = value.as_object();
            let version_ok =
                obj.and_then(|o| o.get("version")).and_then(Value::as_u64) == Some(CACHE_VERSION);
            if version_ok {
                if let Some(existing) = obj
                    .and_then(|o| o.get("servers"))
                    .and_then(Value::as_object)
                {
                    for (name, entry) in existing {
                        if let Ok(entry) = serde_json::from_value::<ServerCacheEntry>(entry.clone())
                        {
                            merged_servers.insert(name.clone(), entry);
                        }
                    }
                }
            }
        }
    }
    for (name, entry) in &cache.servers {
        merged_servers.insert(name.clone(), entry.clone());
    }
    let merged = MetadataCache {
        version: CACHE_VERSION,
        servers: merged_servers,
    };

    // `JSON.stringify(merged, null, 2)`: serde_json's pretty printer is also
    // 2-space, no trailing newline, raw UTF-8 — byte-compatible.
    let serialized = serde_json::to_string_pretty(&merged)
        .map_err(|err| AdapterError::CacheSerialize(err.to_string()))?;
    let tmp_path = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let result =
        std::fs::write(&tmp_path, serialized).and_then(|()| std::fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result?;
    Ok(())
}

/// `stableStringify` (metadata-cache.ts:311-322): recursive
/// key-sorted serialization. Object keys sort by UTF-16 code unit (JS
/// `Array.prototype.sort` default), which differs from byte order only for
/// astral-plane characters.
pub fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(stable_stringify).collect();
            format!("[{}]", rendered.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by_key(|k| k.encode_utf16().collect::<Vec<u16>>());
            let rendered: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        crate::utils::js_json_stringify(&Value::String(k.clone())),
                        stable_stringify(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", rendered.join(","))
        }
        scalar => crate::utils::js_json_stringify(scalar),
    }
}

/// One identity field of `computeServerHash`: JS `undefined` is
/// distinguishable from `null` in the upstream output (`"undefined"` vs
/// `null`), so `None` here means "field absent from the server entry".
fn stable_field(value: Option<Value>) -> String {
    match value {
        Some(v) => stable_stringify(&v),
        None => "undefined".to_string(),
    }
}

/// `computeServerHash` (metadata-cache.ts:81-103): sha256 over the
/// stable-stringified identity field set — transport, env/cwd, url, auth and
/// tool-filter fields after interpolation; lifecycle/idleTimeout/debug and
/// other runtime settings are deliberately excluded.
///
/// Errors mirror the upstream `throw` paths (non-string env values, invalid
/// URL after interpolation, ...); callers treat an error as "cache invalid".
pub fn compute_server_hash(definition: &ServerEntry) -> Result<String, AdapterError> {
    let map = definition.as_map();
    let env = crate::utils::interpolate_env_record(map.get("env"))?.map(Value::Object);
    let headers = crate::utils::interpolate_env_record(map.get("headers"))?.map(Value::Object);
    let socket = crate::utils::resolve_config_path(map.get("socket"))?.map(Value::String);
    let cwd = crate::utils::resolve_config_path(map.get("cwd"))?.map(Value::String);
    let url = crate::utils::resolve_server_url(map.get("url"))?.map(Value::String);
    let bearer_token = crate::utils::resolve_bearer_token(map)?.map(Value::String);

    let raw = |key: &str| map.get(key).cloned();

    // Field set and names must stay item-for-item identical to the upstream
    // identity object (metadata-cache.ts:85-100). Sorting happens here via
    // the same UTF-16 rule as `stableStringify` (all keys are ASCII).
    let fields: Vec<(&str, Option<Value>)> = vec![
        ("command", raw("command")),
        ("args", raw("args")),
        ("socket", socket),
        ("env", env),
        ("cwd", cwd),
        ("url", url),
        ("headers", headers),
        ("auth", raw("auth")),
        ("protocolVersion", raw("protocolVersion")),
        ("bearerToken", bearer_token),
        ("bearerTokenEnv", raw("bearerTokenEnv")),
        ("exposeResources", raw("exposeResources")),
        ("includeTools", raw("includeTools")),
        ("excludeTools", raw("excludeTools")),
    ];
    let mut sorted = fields;
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let rendered: Vec<String> = sorted
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}:{}",
                crate::utils::js_json_stringify(&Value::String(key.to_string())),
                stable_field(value)
            )
        })
        .collect();
    let normalized = format!("{{{}}}", rendered.join(","));

    let digest = sha2::Sha256::digest(normalized.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

/// `isServerCacheValid` (metadata-cache.ts:105-120): hash match + `cachedAt`
/// freshness (7-day TTL; `max_age_ms == 0` disables the age check).
pub fn is_server_cache_valid(
    entry: &ServerCacheEntry,
    definition: &ServerEntry,
    max_age_ms: u64,
    now_ms: u64,
) -> bool {
    let Ok(config_hash) = compute_server_hash(definition) else {
        return false;
    };
    if entry.config_hash != config_hash {
        return false;
    }
    if max_age_ms > 0 && now_ms.saturating_sub(entry.cached_at) > max_age_ms {
        return false;
    }
    true
}

/// `serializeTools` (metadata-cache.ts:238-254), minus the P2 `_meta` UI
/// fields.
pub fn serialize_tools(tools: &[McpTool]) -> Vec<CachedTool> {
    tools
        .iter()
        .filter(|t| !t.name.is_empty())
        .map(|t| CachedTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            ..Default::default()
        })
        .collect()
}

/// `serializeResources` (metadata-cache.ts:256-264).
pub fn serialize_resources(resources: &[McpResource]) -> Vec<CachedResource> {
    resources
        .iter()
        .filter(|r| !r.name.is_empty() && !r.uri.is_empty())
        .map(|r| CachedResource {
            uri: r.uri.clone(),
            name: r.name.clone(),
            description: r.description.clone(),
        })
        .collect()
}

/// `reconstructToolMetadata` (metadata-cache.ts:176-236): rebuild the tool
/// metadata of an unconnected server from its cache entry. The P2
/// `isUiToolVisibleToModel` filter is a no-op here (no `_meta` is cached).
pub fn reconstruct_tool_metadata(
    server_name: &str,
    entry: &ServerCacheEntry,
    prefix: ToolPrefix,
    definition: &ServerEntry,
) -> Vec<ToolMetadata> {
    let mut metadata = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();
    let effective_prefix = resolve_tool_prefix(Some(definition), prefix);

    for tool in &entry.tools {
        if tool.name.is_empty() {
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
        let name = format_tool_name(&tool.name, server_name, effective_prefix);
        if seen_names.contains(&name) {
            continue;
        }
        seen_names.push(name.clone());
        metadata.push(ToolMetadata {
            name,
            original_name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            input_schema: tool.input_schema.clone(),
            resource_uri: None,
        });
    }

    if definition.exposes_resources() {
        for resource in &entry.resources {
            if resource.name.is_empty() || resource.uri.is_empty() {
                continue;
            }
            let base_name = format!("read_{}", resource_name_to_tool_name(&resource.name));
            if !is_tool_allowed(
                &base_name,
                server_name,
                effective_prefix,
                definition.include_tools(),
                definition.exclude_tools(),
            ) {
                continue;
            }
            let name = format_tool_name(&base_name, server_name, effective_prefix);
            if seen_names.contains(&name) {
                continue;
            }
            seen_names.push(name.clone());
            metadata.push(ToolMetadata {
                name,
                original_name: base_name,
                description: resource
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Read resource: {}", resource.uri)),
                resource_uri: Some(resource.uri.clone()),
                input_schema: None,
            });
        }
    }

    metadata
}

/// `PromptMetadata` (types.ts:561-568).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptMetadata {
    pub server_name: String,
    pub original_name: String,
    pub command_name: String,
    pub title: Option<String>,
    pub description: String,
    pub arguments: Vec<CachedPromptArgument>,
}

/// `reconstructPromptMetadata` (metadata-cache.ts:285-309).
pub fn reconstruct_prompt_metadata(
    server_name: &str,
    prompts: &[CachedPrompt],
    prefix: ToolPrefix,
    definition: Option<&ServerEntry>,
) -> Vec<PromptMetadata> {
    let effective_prefix = resolve_tool_prefix(definition, prefix);
    prompts
        .iter()
        .filter(|p| !p.name.is_empty())
        .map(|prompt| PromptMetadata {
            server_name: server_name.to_string(),
            original_name: prompt.name.clone(),
            command_name: format_prompt_command_name(&prompt.name, server_name, effective_prefix),
            title: prompt.title.clone(),
            description: prompt.description.clone().unwrap_or_default(),
            arguments: prompt.arguments.clone().unwrap_or_default(),
        })
        .collect()
}

/// `getMissingConfiguredDirectToolServers` (metadata-cache.ts:148-174):
/// servers whose directTools are enabled (env override > per-server >
/// global) but lack a valid cache entry. Drives the `disableProxyTool`
/// truth table and the P1 bootstrap path.
pub fn get_missing_configured_direct_tool_servers(
    config: &crate::metadata::McpConfig,
    cache: Option<&MetadataCache>,
    env_override: Option<&[String]>,
    now_ms: u64,
) -> Vec<String> {
    let mut missing = Vec::new();
    let global_direct = config
        .settings
        .as_ref()
        .and_then(|s| s.get("directTools"))
        .and_then(Value::as_bool);
    let env_selection = env_override.map(crate::direct::parse_direct_tool_selectors);

    for (server_name, definition) in &config.mcp_servers {
        if definition.is_disabled() {
            continue;
        }
        let has_direct_tools = match &env_selection {
            Some((servers, tools)) => {
                servers.contains(server_name) || tools.contains_key(server_name)
            }
            None => match definition.get("directTools") {
                // JS truthiness: an empty array still counts as configured.
                Some(Value::Bool(v)) => *v,
                Some(Value::Array(_)) => true,
                Some(_) => false,
                None => global_direct.unwrap_or(false),
            },
        };
        if !has_direct_tools {
            continue;
        }
        let valid = cache
            .and_then(|c| c.servers.get(server_name))
            .is_some_and(|entry| {
                is_server_cache_valid(entry, definition, CACHE_MAX_AGE_MS, now_ms)
            });
        if !valid {
            missing.push(server_name.clone());
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(value: Value) -> ServerEntry {
        ServerEntry(value.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn stable_stringify_sorts_keys_recursively() {
        let value = json!({ "b": 1, "a": { "d": [1, 2], "c": null } });
        assert_eq!(
            stable_stringify(&value),
            r#"{"a":{"c":null,"d":[1,2]},"b":1}"#
        );
    }

    #[test]
    fn stable_stringify_matches_js_scalars() {
        assert_eq!(stable_stringify(&json!(null)), "null");
        assert_eq!(stable_stringify(&json!(true)), "true");
        assert_eq!(stable_stringify(&json!("x\"y")), r#""x\"y""#);
        assert_eq!(stable_stringify(&json!([1, "a"])), r#"[1,"a"]"#);
    }

    #[test]
    fn config_hash_is_stable_and_sensitive_to_identity_fields() {
        let a = entry(json!({ "command": "node", "args": ["server.js"] }));
        let a_reordered = entry(json!({ "args": ["server.js"], "command": "node" }));
        let b = entry(json!({ "command": "node", "args": ["other.js"] }));
        let hash_a = compute_server_hash(&a).expect("hash");
        assert_eq!(hash_a, compute_server_hash(&a_reordered).expect("hash"));
        assert_ne!(hash_a, compute_server_hash(&b).expect("hash"));
        assert_eq!(hash_a.len(), 64);
        // Runtime-only fields do NOT change identity (metadata-cache.ts:82-84).
        let mut with_runtime = a.as_map().clone();
        with_runtime.insert("lifecycle".to_string(), json!("eager"));
        with_runtime.insert("idleTimeout".to_string(), json!(0));
        with_runtime.insert("debug".to_string(), json!(true));
        assert_eq!(
            hash_a,
            compute_server_hash(&ServerEntry(with_runtime)).expect("hash")
        );
    }

    #[test]
    fn config_hash_interpolates_env_before_hashing() {
        std::env::set_var("RPI_MCP_HASH_TEST", "resolved-value");
        let templated =
            entry(json!({ "command": "node", "env": { "TOKEN": "$env:RPI_MCP_HASH_TEST" } }));
        let literal = entry(json!({ "command": "node", "env": { "TOKEN": "resolved-value" } }));
        assert_eq!(
            compute_server_hash(&templated).expect("hash"),
            compute_server_hash(&literal).expect("hash")
        );
        std::env::remove_var("RPI_MCP_HASH_TEST");
    }

    #[test]
    fn cache_validity_covers_hash_change_and_ttl() {
        let definition = entry(json!({ "command": "node" }));
        let hash = compute_server_hash(&definition).expect("hash");
        let cache_entry = ServerCacheEntry {
            config_hash: hash,
            cached_at: 1_000,
            ..Default::default()
        };
        assert!(is_server_cache_valid(
            &cache_entry,
            &definition,
            CACHE_MAX_AGE_MS,
            1_000
        ));
        // Expired (7 days + 1ms later).
        assert!(!is_server_cache_valid(
            &cache_entry,
            &definition,
            CACHE_MAX_AGE_MS,
            1_000 + CACHE_MAX_AGE_MS + 1
        ));
        // Hash drift (definition changed).
        let changed = entry(json!({ "command": "deno" }));
        assert!(!is_server_cache_valid(
            &cache_entry,
            &changed,
            CACHE_MAX_AGE_MS,
            1_000
        ));
        // max_age_ms == 0 disables the age check (upstream `maxAgeMs > 0`).
        assert!(is_server_cache_valid(
            &cache_entry,
            &definition,
            0,
            u64::MAX
        ));
    }

    #[test]
    fn load_rejects_foreign_version_and_malformed_files() {
        let dir = std::env::temp_dir().join(format!("rpi-mcp-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_default();
        let path = dir.join("mcp-cache.json");

        std::fs::write(&path, "{ not json").unwrap_or_default();
        assert!(load_metadata_cache(&path).is_none());

        std::fs::write(&path, r#"{ "version": 2, "servers": {} }"#).unwrap_or_default();
        assert!(load_metadata_cache(&path).is_none());

        std::fs::write(&path, r#"{ "version": 1, "servers": {} }"#).unwrap_or_default();
        assert!(load_metadata_cache(&path).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_merges_into_existing_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("rpi-mcp-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_default();
        let path = dir.join("mcp-cache.json");

        let mut first = MetadataCache {
            version: CACHE_VERSION,
            servers: IndexMap::new(),
        };
        first.servers.insert(
            "a".to_string(),
            ServerCacheEntry {
                config_hash: "h1".to_string(),
                cached_at: 1,
                ..Default::default()
            },
        );
        save_metadata_cache(&path, &first).expect("save");

        let mut second = MetadataCache {
            version: CACHE_VERSION,
            servers: IndexMap::new(),
        };
        second.servers.insert(
            "b".to_string(),
            ServerCacheEntry {
                config_hash: "h2".to_string(),
                cached_at: 2,
                ..Default::default()
            },
        );
        save_metadata_cache(&path, &second).expect("save");

        let loaded = load_metadata_cache(&path).expect("load");
        assert!(loaded.servers.contains_key("a"));
        assert!(loaded.servers.contains_key("b"));
        // No temp file survives the atomic rename.
        assert!(std::fs::read_dir(&dir)
            .map(|mut it| it.all(|e| e
                .as_ref()
                .map(|e| { !e.file_name().to_string_lossy().ends_with(".tmp") })
                .unwrap_or(true)))
            .unwrap_or(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconstruct_tool_metadata_round_trips_names() {
        let definition = entry(json!({}));
        let cache_entry = ServerCacheEntry {
            config_hash: "h".to_string(),
            tools: vec![CachedTool {
                name: "list.sims".to_string(),
                description: Some("List".to_string()),
                input_schema: None,
                ..Default::default()
            }],
            resources: vec![CachedResource {
                uri: "mcp://x/y".to_string(),
                name: "Y".to_string(),
                description: None,
            }],
            cached_at: 0,
            ..Default::default()
        };
        let metadata =
            reconstruct_tool_metadata("xcodebuild", &cache_entry, ToolPrefix::Server, &definition);
        let names: Vec<&str> = metadata.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["xcodebuild_list_sims", "xcodebuild_read_y"]);
    }
}
