//! MCP configuration discovery and merge (FR-P0-01).
//!
//! Port of the P0 subset of `config.ts` @ pi-mcp-adapter v2.24.0 (3d953f90):
//! the six config sources, `validateConfig`, `mergeConfigs` /
//! `mergeServerMaps` (including the two credential-safety rules), and the
//! JSONC pre-parse strip (`strip-json-comments` with `trailingCommas`).
//!
//! Path mapping follows ADR-0001 (requirements §3.1): the rpi global layer
//! is `~/.rpi/agent/mcp.json` (overridable with `RPI_CODING_AGENT_DIR`, and
//! per-invocation with `--mcp-config` / `configPath`), the project rpi layer
//! is `<cwd>/.rpi/mcp.json`; the shared standard paths
//! (`~/.config/mcp/mcp.json`, `~/.agents/...`, `<cwd>/.mcp.json`) keep their
//! names. `~/.pi` is never read.
//!
//! P0 scope cuts [documented VARIANTs / later layers]:
//! - `imports` is parsed and preserved through merges but NOT expanded
//!   (FR-P0-01 VARIANT; host config import is P2).
//! - `hostConfigDiscovery`, Agent Plugins (`agent-plugin-loader.ts`),
//!   RepoPrompt detection and the write-side helpers are P1/P2 and absent.
//! - Home directory is `HOME` / `USERPROFILE` only (see `utils.rs`).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value};
use tracing::warn;

use crate::metadata::{McpConfig, ServerEntry};

/// `~/.config/mcp/mcp.json` (config.ts:12) — shared standard, name kept.
const GENERIC_GLOBAL_CONFIG_PATH: &str = ".config/mcp/mcp.json";
/// `~/.agents/mcp.json` and `~/.agents/mcp/mcp.json` (config.ts:13-16).
const AGENTS_GLOBAL_CONFIG_PATHS: [&str; 2] = [".agents/mcp.json", ".agents/mcp/mcp.json"];
/// `<cwd>/.mcp.json` (config.ts:17).
const PROJECT_CONFIG_NAME: &str = ".mcp.json";
/// `<cwd>/.rpi/mcp.json` — upstream `.pi/mcp.json` (config.ts:18), renamed
/// per ADR-0001.
const PROJECT_RPI_CONFIG_NAME: &str = ".rpi/mcp.json";

/// `getAgentDir` (agent-dir.ts:5-17) with the ADR-0001 mapping:
/// `RPI_CODING_AGENT_DIR` overrides, default `~/.rpi/agent`. The upstream
/// `PI_PACKAGE_DIR` brand probing is not ported (rpi has one brand).
pub fn get_agent_dir() -> PathBuf {
    if let Some(configured) = std::env::var("RPI_CODING_AGENT_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        if configured == "~" {
            if let Some(home) = crate::utils::home_dir() {
                return home;
            }
        } else if let Some(rest) = configured.strip_prefix("~/") {
            if let Some(home) = crate::utils::home_dir() {
                return home.join(rest);
            }
        } else {
            return resolve_path(&configured);
        }
    }
    match crate::utils::home_dir() {
        Some(home) => home.join(".rpi").join("agent"),
        None => PathBuf::from(".rpi").join("agent"),
    }
}

/// `path.resolve` for one segment: absolute paths pass through; relative
/// paths join onto the process cwd; the result is lexically normalized
/// (`.` / `..` collapsed without touching the filesystem, like Node's
/// `path.resolve`).
pub fn resolve_path(path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(candidate)
    };
    normalize_lexical(&absolute)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `getPiGlobalConfigPath` (config.ts:167-169): the rpi global layer path,
/// overridable per invocation (upstream `--mcp-config` / `configPath`).
pub fn get_rpi_global_config_path(override_path: Option<&str>) -> PathBuf {
    match override_path {
        Some(path) => resolve_path(path),
        None => get_agent_dir().join("mcp.json"),
    }
}

/// `getProjectConfigPath` (config.ts:175-177).
pub fn get_project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_CONFIG_NAME)
}

/// `getProjectPiConfigPath` (config.ts:179-181) — rpi side: `.rpi/mcp.json`.
pub fn get_project_rpi_config_path(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_RPI_CONFIG_NAME)
}

/// One discovery source (upstream `ConfigSourceSpec`, config.ts:84-93). Only
/// the fields the P0 loader needs are modeled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub id: &'static str,
    pub read_path: PathBuf,
    pub write_path: PathBuf,
}

/// `getConfigSources` (config.ts:389-457): the six sources in ascending
/// precedence, with the upstream path-equality skip rules (a source whose
/// path coincides with a higher-precedence one is only read once).
pub fn get_config_sources(override_path: Option<&str>, cwd: &Path) -> Vec<ConfigSource> {
    let Some(home) = crate::utils::home_dir() else {
        // Without a home directory the global sources are unresolvable; the
        // project sources still apply.
        return project_sources(override_path, cwd);
    };
    let generic_global = home.join(GENERIC_GLOBAL_CONFIG_PATH);
    let user_path = get_rpi_global_config_path(override_path);
    let project_path = get_project_config_path(cwd);
    let project_rpi_path = get_project_rpi_config_path(cwd);

    let mut sources = Vec::new();
    if generic_global != user_path {
        sources.push(ConfigSource {
            id: "shared-global",
            read_path: generic_global.clone(),
            write_path: user_path.clone(),
        });
    }
    for (index, agents_path) in AGENTS_GLOBAL_CONFIG_PATHS.iter().enumerate() {
        let agents_path = home.join(agents_path);
        if agents_path == user_path || agents_path == generic_global {
            continue;
        }
        sources.push(ConfigSource {
            id: if index == 0 {
                "agents-global"
            } else {
                "agents-nested-global"
            },
            read_path: agents_path,
            write_path: user_path.clone(),
        });
    }
    sources.push(ConfigSource {
        id: "rpi-global", // upstream id: "pi-global"
        read_path: user_path.clone(),
        write_path: user_path.clone(),
    });
    sources.extend(project_sources_with_paths(
        &user_path,
        project_path,
        project_rpi_path,
    ));
    sources
}

fn project_sources(override_path: Option<&str>, cwd: &Path) -> Vec<ConfigSource> {
    let user_path = get_rpi_global_config_path(override_path);
    sources_extend_project(
        Vec::new(),
        &user_path,
        get_project_config_path(cwd),
        get_project_rpi_config_path(cwd),
    )
}

fn project_sources_with_paths(
    user_path: &Path,
    project_path: PathBuf,
    project_rpi_path: PathBuf,
) -> Vec<ConfigSource> {
    sources_extend_project(Vec::new(), user_path, project_path, project_rpi_path)
}

fn sources_extend_project(
    mut sources: Vec<ConfigSource>,
    user_path: &Path,
    project_path: PathBuf,
    project_rpi_path: PathBuf,
) -> Vec<ConfigSource> {
    if project_path != user_path {
        sources.push(ConfigSource {
            id: "shared-project",
            read_path: project_path.clone(),
            write_path: project_path.clone(),
        });
    }
    if project_rpi_path != user_path && project_rpi_path != project_path {
        sources.push(ConfigSource {
            id: "rpi-project", // upstream id: "pi-project"
            read_path: project_rpi_path.clone(),
            write_path: project_rpi_path,
        });
    }
    sources
}

/// JSONC pre-parse strip: `strip-json-comments` semantics with
/// `trailingCommas: true` (config.ts:577-579). Comments are replaced with
/// whitespace (newlines preserved so parse-error positions stay meaningful);
/// a comma whose next significant character is `]` or `}` is likewise
/// blanked.
pub fn strip_json_comments(raw: &str) -> String {
    let blanked = blank_comments(raw);
    blank_trailing_commas(&blanked)
}

fn blank_comments(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out: Vec<char> = chars.clone();
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && (chars[i + 1] == '/' || chars[i + 1] == '*') {
            let block = chars[i + 1] == '*';
            out[i] = ' ';
            out[i + 1] = ' ';
            i += 2;
            loop {
                if i >= chars.len() {
                    break;
                }
                let c = chars[i];
                if block && c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 2;
                    break;
                }
                if !block && (c == '\n' || c == '\r') {
                    // Line comments end at the newline, which is kept.
                    break;
                }
                if c != '\n' && c != '\r' {
                    out[i] = ' ';
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out.into_iter().collect()
}

fn blank_trailing_commas(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out: Vec<char> = chars.clone();
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_string = true,
            ',' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && (chars[j] == ']' || chars[j] == '}') {
                    out[i] = ' ';
                }
            }
            _ => {}
        }
        i += 1;
    }
    out.into_iter().collect()
}

/// `parseJsonConfig` (config.ts:577-579).
pub fn parse_json_config(raw: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(&strip_json_comments(raw))
}

/// `validateConfig` (config.ts:640-650): non-record roots become an empty
/// config; `mcpServers` (or the legacy `mcp-servers` key) keeps only record
/// entries; `imports` and `settings` pass through unchecked.
pub fn validate_config(raw: &Value) -> McpConfig {
    let Some(obj) = raw.as_object() else {
        return McpConfig::default();
    };
    let servers_value = obj.get("mcpServers").or_else(|| obj.get("mcp-servers"));
    let mut mcp_servers = IndexMap::new();
    if let Some(servers) = servers_value.and_then(Value::as_object) {
        for (name, entry) in servers {
            if let Some(entry) = entry.as_object() {
                mcp_servers.insert(name.clone(), ServerEntry(entry.clone()));
            }
        }
    }
    let imports = obj.get("imports").and_then(Value::as_array).map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    // Upstream `Array.isArray(raw.imports) ? { imports: raw.imports } : {}`
    // keeps non-string entries; P0 preserves only strings (the ImportKind
    // union is all strings; host import expansion is P2).
    let settings = obj.get("settings").and_then(Value::as_object).cloned();
    McpConfig {
        mcp_servers,
        imports,
        settings,
    }
}

/// `mergeImports` (config.ts:520-524): concatenation with first-wins
/// dedupe; `None` when both sides are empty/absent.
fn merge_imports(left: Option<&Vec<String>>, right: Option<&Vec<String>>) -> Option<Vec<String>> {
    let mut merged: Vec<String> = Vec::new();
    for source in [left, right].into_iter().flatten() {
        for kind in source {
            if !merged.contains(kind) {
                merged.push(kind.clone());
            }
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// Credential-bearing fields whose value is bound to a specific server
/// `url` (config.ts:469-474).
const URL_BOUND_AUTH_FIELDS: [&str; 3] = ["headers", "bearerToken", "bearerTokenEnv"];

/// Fields dropped when an override switches a server to `socket` transport
/// (config.ts:495-500).
const SOCKET_SWITCH_CLEARED_FIELDS: [&str; 9] = [
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "headers",
    "auth",
    "bearerToken",
    "bearerTokenEnv",
];

/// `mergeServerMaps` (config.ts:476-518) — per-field fold with the two
/// SECURITY rules:
///
/// 1. **Transport switch** (config.ts:494-505): an override supplying
///    `socket` clears all command/url-side fields from the inherited entry;
///    an override supplying `command`/`url` over an inherited `socket`
///    entry drops that `socket`.
/// 2. **Credential/url binding** (config.ts:483-514): when the override
///    changes `url`, inherited `headers`/`bearerToken`/`bearerTokenEnv` (and
///    `oauth`, unless it is exactly `false`) are stripped BEFORE the
///    per-field merge, so credentials bound to the old url cannot follow the
///    server to an attacker-controlled url. Auth re-supplied by the override
///    still applies (it is merged last).
pub fn merge_server_maps(
    base: &IndexMap<String, ServerEntry>,
    next: &IndexMap<String, ServerEntry>,
) -> IndexMap<String, ServerEntry> {
    let mut merged = base.clone();
    for (name, definition) in next {
        let existing = merged.get(name);
        let mut base_entry: Option<Map<String, Value>> = None;
        if let Some(existing) = existing {
            let mut entry = existing.as_map().clone();
            let definition_socket_is_string =
                definition.get("socket").is_some_and(Value::is_string);
            // JS truthiness for the reverse direction: `existing?.socket` —
            // null/false/0/"" do NOT trigger the clear, other values do.
            let existing_socket_truthy = match existing.get("socket") {
                Some(Value::String(s)) => !s.is_empty(),
                Some(Value::Bool(b)) => *b,
                Some(Value::Null) | None => false,
                Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
                Some(_) => true,
            };
            let definition_has_command_or_url =
                definition.get("command").is_some_and(Value::is_string)
                    || definition.get("url").is_some_and(Value::is_string);
            if definition_socket_is_string {
                for field in SOCKET_SWITCH_CLEARED_FIELDS {
                    entry.shift_remove(field);
                }
            } else if existing_socket_truthy && definition_has_command_or_url {
                entry.shift_remove("socket");
            }
            base_entry = Some(entry);
        }
        if let (Some(existing), Some(base)) = (existing, base_entry.as_mut()) {
            if let Some(Value::String(new_url)) = definition.get("url") {
                let url_changed = existing.get("url") != Some(&Value::String(new_url.clone()));
                if url_changed {
                    for field in URL_BOUND_AUTH_FIELDS {
                        base.shift_remove(field);
                    }
                    if base.get("oauth") != Some(&Value::Bool(false)) {
                        base.shift_remove("oauth");
                    }
                }
            }
        }
        let mut merged_entry = base_entry.unwrap_or_default();
        for (key, value) in definition.as_map() {
            merged_entry.insert(key.clone(), value.clone());
        }
        merged.insert(name.clone(), ServerEntry(merged_entry));
    }
    merged
}

/// `mergeConfigs` (config.ts:459-467).
pub fn merge_configs(base: &McpConfig, next: &McpConfig) -> McpConfig {
    let imports = merge_imports(base.imports.as_ref(), next.imports.as_ref());
    let settings = match &next.settings {
        Some(next_settings) => {
            let mut merged = base.settings.clone().unwrap_or_default();
            for (key, value) in next_settings {
                merged.insert(key.clone(), value.clone());
            }
            Some(merged)
        }
        None => base.settings.clone(),
    };
    McpConfig {
        mcp_servers: merge_server_maps(&base.mcp_servers, &next.mcp_servers),
        imports,
        settings,
    }
}

/// `readValidatedConfig` (config.ts:629-638): missing files yield `None`;
/// malformed files warn (upstream `console.warn` -> `tracing::warn`, never
/// stdout) and are skipped.
pub fn read_validated_config(path: &Path) -> Option<McpConfig> {
    if !path.exists() {
        return None;
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to read MCP config");
            return None;
        }
    };
    match parse_json_config(&raw) {
        Ok(value) => Some(validate_config(&value)),
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to load MCP config");
            None
        }
    }
}

/// `loadMcpConfig` (config.ts:293-311), P0 cut: fold the six sources in
/// ascending precedence. Upstream's `expandImports`, host-config discovery
/// and Agent Plugin layers are not expanded here (VARIANT / P2); `imports`
/// survives on the returned config.
pub fn load_mcp_config(override_path: Option<&str>, cwd: &Path) -> McpConfig {
    let mut config = McpConfig::default();
    for source in get_config_sources(override_path, cwd) {
        let Some(loaded) = read_validated_config(&source.read_path) else {
            continue;
        };
        config = merge_configs(&config, &loaded);
    }
    config
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn servers(value: Value) -> IndexMap<String, ServerEntry> {
        validate_config(&json!({ "mcpServers": value })).mcp_servers
    }

    #[test]
    fn strip_comments_and_trailing_commas() {
        let raw =
            "{\n  // line comment\n  \"a\": 1, /* block\n  comment */ \"b\": \"http://x\", \n}";
        let parsed = parse_json_config(raw).expect("JSONC parses");
        assert_eq!(parsed, json!({ "a": 1, "b": "http://x" }));
    }

    #[test]
    fn strip_preserves_comment_like_strings() {
        let raw = r#"{ "url": "https://a.b/c", "pattern": "/* not comment */" }"#;
        let parsed = parse_json_config(raw).expect("strings survive");
        assert_eq!(
            parsed,
            json!({ "url": "https://a.b/c", "pattern": "/* not comment */" })
        );
    }

    #[test]
    fn validate_config_drops_malformed_entries() {
        // config.test.ts: "drops malformed server entries at the config boundary"
        let config = validate_config(&json!({
            "mcpServers": {
                "valid": { "command": "node" },
                "nullEntry": null,
                "listEntry": [],
                "stringEntry": "node",
            }
        }));
        assert_eq!(
            config.mcp_servers,
            servers(json!({ "valid": { "command": "node" } }))
        );
    }

    #[test]
    fn validate_config_accepts_legacy_mcp_servers_key() {
        let config = validate_config(&json!({ "mcp-servers": { "a": { "command": "x" } } }));
        assert!(config.mcp_servers.contains_key("a"));
    }

    #[test]
    fn merge_is_per_field_with_latter_wins() {
        let base = servers(json!({
            "shared": { "command": "shared", "args": ["--stdio"], "env": { "TOKEN": "shared-token" } }
        }));
        let next = servers(json!({ "shared": { "directTools": true } }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["shared"].as_map(),
            &json!({
                "command": "shared",
                "args": ["--stdio"],
                "env": { "TOKEN": "shared-token" },
                "directTools": true,
            })
            .as_object()
            .cloned()
            .unwrap_or_default()
        );
    }

    // SECURITY专测 — config.test.ts "drops inherited auth when a
    // higher-precedence override changes the url without supplying new auth".

    const URL_A: &str = "https://litellm.internal/mcp/";
    const URL_B: &str = "https://attacker.example/mcp/";

    #[test]
    fn security_same_url_keeps_inherited_auth() {
        let base = servers(
            json!({ "litellm": { "url": URL_A, "headers": { "Authorization": "Bearer secret-vk" } } }),
        );
        let next = servers(json!({ "litellm": { "url": URL_A, "directTools": true } }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({
                "url": URL_A,
                "headers": { "Authorization": "Bearer secret-vk" },
                "directTools": true,
            })
            .as_object()
            .unwrap()
        );
    }

    #[test]
    fn security_url_change_strips_inherited_auth() {
        let base = servers(
            json!({ "litellm": { "url": URL_A, "headers": { "Authorization": "Bearer secret-vk" } } }),
        );
        let next = servers(json!({ "litellm": { "url": URL_B } }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({ "url": URL_B }).as_object().unwrap()
        );
    }

    #[test]
    fn security_url_change_keeps_only_override_auth() {
        let base = servers(
            json!({ "litellm": { "url": URL_A, "headers": { "Authorization": "Bearer secret-vk" } } }),
        );
        let next = servers(
            json!({ "litellm": { "url": URL_B, "headers": { "Authorization": "Bearer override-token" } } }),
        );
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({ "url": URL_B, "headers": { "Authorization": "Bearer override-token" } })
                .as_object()
                .unwrap()
        );
    }

    #[test]
    fn security_url_change_strips_bearer_token_and_env() {
        let base = servers(json!({
            "litellm": {
                "url": URL_A,
                "headers": { "Authorization": "Bearer ${LITELLM_API_KEY}" },
                "bearerTokenEnv": "LITELLM_API_KEY",
            }
        }));
        let next = servers(json!({ "litellm": { "url": URL_B } }));
        let merged = merge_server_maps(&base, &next);
        let entry = &merged["litellm"];
        assert_eq!(entry.get("url").and_then(Value::as_str), Some(URL_B));
        assert!(entry.get("headers").is_none());
        assert!(entry.get("bearerTokenEnv").is_none());
        let serialized = serde_json::to_string(entry.as_map()).unwrap_or_default();
        assert!(!serialized.contains("LITELLM_API_KEY"));
    }

    #[test]
    fn security_url_change_strips_bearer_token_literal() {
        let base =
            servers(json!({ "litellm": { "url": URL_A, "bearerToken": "secret-bearer-token" } }));
        let next = servers(json!({ "litellm": { "url": URL_B } }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({ "url": URL_B }).as_object().unwrap()
        );
    }

    #[test]
    fn security_url_change_strips_oauth_but_preserves_explicit_false() {
        let base = servers(
            json!({ "litellm": { "url": URL_A, "oauth": { "clientId": "client", "clientSecret": "oauth-client-secret" } } }),
        );
        let next = servers(json!({ "litellm": { "url": URL_B } }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({ "url": URL_B }).as_object().unwrap()
        );

        let base = servers(json!({ "litellm": { "url": URL_A, "oauth": false } }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({ "url": URL_B, "oauth": false }).as_object().unwrap()
        );
    }

    #[test]
    fn security_three_source_laundering_is_prevented() {
        // config.test.ts: the accumulated (folded) entry's url drives the
        // strip decision — auth re-supplied by a middle source without a url
        // must still be stripped when the top source repoints the url.
        let low = servers(
            json!({ "litellm": { "url": URL_A, "headers": { "Authorization": "Bearer secret-vk" } } }),
        );
        let mid =
            servers(json!({ "litellm": { "headers": { "Authorization": "Bearer secret-vk" } } }));
        let top = servers(json!({ "litellm": { "url": URL_B } }));
        let merged = merge_server_maps(&merge_server_maps(&low, &mid), &top);
        assert_eq!(
            merged["litellm"].as_map(),
            json!({ "url": URL_B }).as_object().unwrap()
        );
    }

    #[test]
    fn security_socket_switch_clears_opposing_transport_fields() {
        // config.test.ts: "replaces transport-specific fields when an
        // override switches to or from a socket"
        let base = servers(json!({
            "toSocket": { "command": "old", "args": ["--old"], "env": { "OLD": "1" }, "cwd": "/old" },
            "toCommand": { "socket": "/old.sock" },
            "toUrl": { "socket": "/old.sock" },
        }));
        let next = servers(json!({
            "toSocket": { "socket": "/shared.sock" },
            "toCommand": { "command": "new" },
            "toUrl": { "url": "https://example.test/mcp" },
        }));
        let merged = merge_server_maps(&base, &next);
        assert_eq!(
            merged["toSocket"].as_map(),
            json!({ "socket": "/shared.sock" }).as_object().unwrap()
        );
        assert_eq!(
            merged["toCommand"].as_map(),
            json!({ "command": "new" }).as_object().unwrap()
        );
        assert_eq!(
            merged["toUrl"].as_map(),
            json!({ "url": "https://example.test/mcp" })
                .as_object()
                .unwrap()
        );
    }

    #[test]
    fn settings_merge_key_level_latter_wins() {
        // config.test.ts: "loads standard MCP files first, then Pi overrides"
        let base = McpConfig {
            settings: Some(
                json!({ "idleTimeout": 5, "requestTimeoutMs": 1500, "showStatusIcon": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        let next = McpConfig {
            settings: Some(
                json!({ "toolPrefix": "short", "directTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        let top = McpConfig {
            settings: Some(
                json!({ "showStatusIcon": false })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        let merged = merge_configs(&merge_configs(&base, &next), &top);
        assert_eq!(
            merged.settings,
            json!({
                "idleTimeout": 5,
                "requestTimeoutMs": 1500,
                "showStatusIcon": false,
                "toolPrefix": "short",
                "directTools": true,
            })
            .as_object()
            .cloned()
        );
    }

    #[test]
    fn imports_merge_dedupes_and_stays_unexpanded() {
        let base = McpConfig {
            imports: Some(vec!["cursor".to_string()]),
            ..Default::default()
        };
        let next = McpConfig {
            imports: Some(vec!["cursor".to_string(), "codex".to_string()]),
            ..Default::default()
        };
        let merged = merge_configs(&base, &next);
        assert_eq!(
            merged.imports,
            Some(vec!["cursor".to_string(), "codex".to_string()])
        );
        assert!(merged.mcp_servers.is_empty());
    }
}
