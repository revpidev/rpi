//! MCP server configuration (port of
//! `packages/coding-agent/src/extensions/mcp/config.ts` and the config half
//! of `core/mcp-servers.ts` @ a13d35a74).
//!
//! Servers are read from `mcp.json` in the agent directory and, for trusted
//! projects, from `<project>/.rpi/mcp.json`. Both use the `mcpServers`
//! shape shared by other MCP clients. Project entries replace global
//! entries with the same name; a repository cannot pick where a credential
//! goes (`auth` is global-only).

use std::path::Path;

use rpi_ext_host::types::ToolExposure;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// `.rpi` project directory name.
pub const CONFIG_DIR_NAME: &str = crate::config::CONFIG_DIR_NAME;

/// `McpExposure` (mcp-servers.ts:11): server-side exposure tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpExposure {
    Codemode,
    Deferred,
    Direct,
    Hidden,
}

impl McpExposure {
    pub const ALL: [McpExposure; 4] = [
        McpExposure::Codemode,
        McpExposure::Deferred,
        McpExposure::Direct,
        McpExposure::Hidden,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codemode" | "codemode-deferred" => Some(McpExposure::Codemode),
            "deferred" => Some(McpExposure::Deferred),
            "direct" => Some(McpExposure::Direct),
            "hidden" => Some(McpExposure::Hidden),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            McpExposure::Codemode => "codemode",
            McpExposure::Deferred => "deferred",
            McpExposure::Direct => "direct",
            McpExposure::Hidden => "hidden",
        }
    }

    /// `toToolExposure` (tools.ts:38): `codemode` maps to the host's
    /// `deferred` tier (registered, not declared; the codemode tool reaches
    /// it). `direct`/`hidden` map through.
    pub fn to_tool_exposure(self) -> ToolExposure {
        match self {
            McpExposure::Codemode => ToolExposure::Codemode,
            McpExposure::Deferred => ToolExposure::Deferred,
            McpExposure::Direct => ToolExposure::Direct,
            McpExposure::Hidden => ToolExposure::Hidden,
        }
    }
}

/// `McpOAuthConfig` (mcp-servers.ts:94).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// May reference environment variables or commands; resolved at use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// `client_name` for dynamic client registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// Authorization server metadata document used instead of discovery
    /// (v1.0.0 `d850edee9`); https except on loopback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

/// Common server options (mcp-servers.ts:29).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerCommon {
    /// Default: `codemode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_exposure: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Per-request timeout in seconds (default 60).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

/// `McpStdioServerConfig` (mcp-servers.ts:61).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStdioServerConfig {
    #[serde(flatten)]
    pub common: McpServerCommon,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `McpHttpServerConfig` (mcp-servers.ts:109).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpHttpServerConfig {
    #[serde(flatten)]
    pub common: McpServerCommon,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
    /// Reuse a pi provider's `/login` token as bearer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuthConfig>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `auth: { provider }` (mcp-servers.ts:118).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpAuthConfig {
    pub provider: String,
}

/// `McpServerConfig` (mcp-servers.ts:125).
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerConfig {
    Stdio(McpStdioServerConfig),
    Http(McpHttpServerConfig),
}

impl McpServerConfig {
    pub fn common(&self) -> &McpServerCommon {
        match self {
            McpServerConfig::Stdio(config) => &config.common,
            McpServerConfig::Http(config) => &config.common,
        }
    }

    pub fn enabled(&self) -> bool {
        self.common().enabled.unwrap_or(true)
    }

    pub fn exposure(&self) -> McpExposure {
        self.common()
            .exposure
            .as_deref()
            .and_then(McpExposure::parse)
            .unwrap_or(McpExposure::Codemode)
    }

    pub fn timeout_ms(&self) -> u64 {
        self.common()
            .timeout
            .filter(|timeout| *timeout > 0.0)
            .map(|timeout| (timeout * 1000.0) as u64)
            .unwrap_or(60_000)
    }

    /// URL for HTTP servers.
    pub fn url(&self) -> Option<&str> {
        match self {
            McpServerConfig::Http(config) => Some(&config.url),
            McpServerConfig::Stdio(_) => None,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            McpServerConfig::Stdio(config) => serde_json::to_value(config).unwrap_or(Value::Null),
            McpServerConfig::Http(config) => serde_json::to_value(config).unwrap_or(Value::Null),
        }
    }
}

/// `McpScope` (config.ts:26): where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    Global,
    Project,
    Extension,
}

impl McpScope {
    pub fn as_str(self) -> &'static str {
        match self {
            McpScope::Global => "global",
            McpScope::Project => "project",
            McpScope::Extension => "extension",
        }
    }
}

/// `McpServerEntry` (config.ts:20).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerEntry {
    pub name: String,
    pub config: McpServerConfig,
    /// Config file that defined the entry, or the extension path.
    pub source: String,
    pub scope: Option<McpScope>,
}

/// `LoadedMcpConfig` (config.ts:33).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedMcpConfig {
    pub servers: Vec<McpServerEntry>,
    /// Activate codemode when `codemode` servers connect (default true).
    pub auto_enable_codemode: Option<bool>,
    pub errors: Vec<String>,
}

/// `mcpNamespace` (mcp-servers.ts:133): `mcp__<server>` with `-` → `_`.
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// `toolPatternRegExp` (mcp-servers.ts:213): `*` matches any characters,
/// everything else is literal.
fn tool_pattern_matches(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return pattern == value;
    };
    if !value.starts_with(first) {
        return false;
    }
    let mut rest = &value[first.len()..];
    let mut last = first;
    for part in parts {
        last = part;
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    value.ends_with(last)
}

/// `getMcpToolExposure` (mcp-servers.ts:225): an exact `toolExposure` key
/// wins; otherwise the first matching `*` pattern; otherwise the server's
/// exposure (default `codemode`).
pub fn get_mcp_tool_exposure(config: &McpServerConfig, tool_name: &str) -> McpExposure {
    let overrides = config.common().tool_exposure.as_ref();
    if let Some(overrides) = overrides {
        if let Some(value) = overrides.get(tool_name).and_then(Value::as_str)
            && let Some(exposure) = McpExposure::parse(value)
        {
            return exposure;
        }
        for (pattern, value) in overrides {
            if pattern.contains('*')
                && let Some(exposure) = value.as_str().and_then(McpExposure::parse)
                && tool_pattern_matches(pattern, tool_name)
            {
                return exposure;
            }
        }
    }
    config.exposure()
}

/// `LOOPBACK_HOSTS` (mcp-servers.ts:90).
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// `isLoopbackRedirectUri` (mcp-servers.ts:93): an http URI on a loopback
/// host without query or fragment.
pub fn is_loopback_redirect_uri(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && LOOPBACK_HOSTS
            .iter()
            .any(|host| host.trim_matches(['[', ']']) == url.host_str().unwrap_or_default())
        && url.query().is_none()
        && url.fragment().is_none()
}

fn is_server_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn is_string_record(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.values().all(Value::is_string))
}

fn is_exposure(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| McpExposure::parse(text).is_some())
}

fn resolve_exposure_alias(value: &Value) -> Value {
    match value.as_str() {
        Some("codemode-deferred") => Value::String("codemode".to_owned()),
        _ => value.clone(),
    }
}

/// `resolveExposureAliases` (mcp-servers.ts:200).
fn resolve_exposure_aliases(mut value: Map<String, Value>) -> Map<String, Value> {
    if let Some(exposure) = value.get("exposure").cloned() {
        value.insert("exposure".to_owned(), resolve_exposure_alias(&exposure));
    }
    if let Some(tool_exposure) = value.get("toolExposure").and_then(Value::as_object) {
        let resolved: Map<String, Value> = tool_exposure
            .iter()
            .map(|(tool, exposure)| (tool.clone(), resolve_exposure_alias(exposure)))
            .collect();
        value.insert("toolExposure".to_owned(), Value::Object(resolved));
    }
    value
}

fn validate_oauth(value: Option<&Value>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(map) = value.as_object() else {
        return Err("oauth must be an object".to_owned());
    };
    if map.get("clientId").is_some_and(|value| !value.is_string()) {
        return Err("oauth.clientId must be a string".to_owned());
    }
    if map
        .get("clientSecret")
        .is_some_and(|value| !value.is_string())
    {
        return Err("oauth.clientSecret must be a string".to_owned());
    }
    let port = map.get("callbackPort");
    if let Some(port) = port {
        let valid = port
            .as_u64()
            .is_some_and(|port| (1..=65535).contains(&port));
        if !valid {
            return Err("oauth.callbackPort must be a port number".to_owned());
        }
    }
    if let Some(callback_url) = map.get("callbackUrl") {
        let Some(callback_url) = callback_url.as_str() else {
            return Err("oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment".to_owned());
        };
        if !is_loopback_redirect_uri(callback_url) {
            return Err("oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment".to_owned());
        }
        let url_port = url::Url::parse(callback_url)
            .ok()
            .and_then(|url| url.port());
        if let (Some(url_port), Some(port)) = (url_port, port.and_then(Value::as_u64))
            && url_port as u64 != port
        {
            return Err("oauth.callbackUrl and oauth.callbackPort name different ports".to_owned());
        }
    }
    if map.get("scope").is_some_and(|value| !value.is_string()) {
        return Err("oauth.scope must be a string".to_owned());
    }
    if let Some(client_name) = map.get("clientName") {
        let valid = client_name
            .as_str()
            .is_some_and(|name| !name.trim().is_empty());
        if !valid {
            return Err("oauth.clientName must be a non-empty string".to_owned());
        }
    }
    if let Some(metadata_url) = map.get("authServerMetadataUrl") {
        let url = metadata_url
            .as_str()
            .and_then(|value| url::Url::parse(value).ok());
        let valid = url.as_ref().is_some_and(|url| {
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && LOOPBACK_HOSTS.iter().any(|host| {
                        host.trim_matches(['[', ']']) == url.host_str().unwrap_or_default()
                    }))
        });
        if !valid {
            return Err(
                "oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// `validateMcpServerConfig` (mcp-servers.ts:246): a validated copy with
/// exposure aliases resolved, or an error message.
pub fn validate_mcp_server_config(name: &str, raw: &Value) -> Result<McpServerConfig, String> {
    if !is_server_name(name) {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(raw) = raw.as_object() else {
        return Err(format!("server \"{name}\" must be an object"));
    };
    let value = resolve_exposure_aliases(raw.clone());
    let kind = value.get("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return Err(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    let exposures = "\"codemode\", \"deferred\", \"direct\", \"hidden\"";
    if let Some(exposure) = value.get("exposure")
        && !is_exposure(exposure)
    {
        return Err(format!(
            "server \"{name}\": exposure must be one of {exposures}"
        ));
    }
    if let Some(tool_exposure) = value.get("toolExposure") {
        let Some(map) = tool_exposure.as_object() else {
            return Err(format!(
                "server \"{name}\": toolExposure must map tool names to exposures"
            ));
        };
        for (tool, exposure) in map {
            if !is_exposure(exposure) {
                return Err(format!(
                    "server \"{name}\": toolExposure \"{tool}\" must be one of {exposures}"
                ));
            }
        }
    }
    if value
        .get("enabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(format!("server \"{name}\": enabled must be a boolean"));
    }
    if value
        .get("description")
        .is_some_and(|value| !value.is_string())
    {
        return Err(format!("server \"{name}\": description must be a string"));
    }
    if let Some(timeout) = value.get("timeout") {
        let valid = timeout.as_f64().is_some_and(|timeout| timeout > 0.0);
        if !valid {
            return Err(format!(
                "server \"{name}\": timeout must be a positive number of seconds"
            ));
        }
    }
    // HTTP: a URL with an optional `"http"`/`"streamable-http"` type.
    let is_http = value.get("url").is_some_and(Value::is_string)
        && matches!(kind, None | Some("http") | Some("streamable-http"));
    if is_http {
        let url = value.get("url").and_then(Value::as_str).unwrap_or_default();
        let valid = url::Url::parse(url)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "http" | "https"));
        if !valid {
            return Err(format!(
                "server \"{name}\": url must be an http or https URL"
            ));
        }
        if let Some(headers) = value.get("headers")
            && !is_string_record(headers)
        {
            return Err(format!(
                "server \"{name}\": headers must map names to strings"
            ));
        }
        validate_oauth(value.get("oauth"))
            .map_err(|error| format!("server \"{name}\": {error}"))?;
        if let Some(auth) = value.get("auth") {
            let valid = auth
                .as_object()
                .and_then(|auth| auth.get("provider"))
                .and_then(Value::as_str)
                .is_some_and(|provider| !provider.is_empty());
            if !valid {
                return Err(format!(
                    "server \"{name}\": auth.provider must be a provider name"
                ));
            }
            let url = url::Url::parse(url).expect("validated above");
            let loopback = LOOPBACK_HOSTS
                .iter()
                .any(|host| host.trim_matches(['[', ']']) == url.host_str().unwrap_or_default());
            if url.scheme() != "https" && !loopback {
                return Err(format!(
                    "server \"{name}\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]"
                ));
            }
        }
        let config: McpHttpServerConfig = serde_json::from_value(Value::Object(value))
            .map_err(|error| format!("server \"{name}\": {error}"))?;
        return Ok(McpServerConfig::Http(config));
    }
    // stdio: a command with an optional `"stdio"` type.
    if value.get("command").is_some_and(Value::is_string) && matches!(kind, None | Some("stdio")) {
        if let Some(args) = value.get("args") {
            let valid = args
                .as_array()
                .is_some_and(|args| args.iter().all(Value::is_string));
            if !valid {
                return Err(format!(
                    "server \"{name}\": args must be an array of strings"
                ));
            }
        }
        if let Some(env) = value.get("env")
            && !is_string_record(env)
        {
            return Err(format!("server \"{name}\": env must map names to strings"));
        }
        if value.get("cwd").is_some_and(|value| !value.is_string()) {
            return Err(format!("server \"{name}\": cwd must be a string"));
        }
        let config: McpStdioServerConfig = serde_json::from_value(Value::Object(value))
            .map_err(|error| format!("server \"{name}\": {error}"))?;
        return Ok(McpServerConfig::Stdio(config));
    }
    Err(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

/// `readConfigFile` (config.ts:55) into one loaded config.
fn read_config_file(path: &Path, scope: McpScope, state: &mut LoadedMcpConfig) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            state.errors.push(format!("{}: {error}", path.display()));
            return;
        }
    };
    let Some(parsed) = parsed.as_object() else {
        state.errors.push(format!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        ));
        return;
    };
    if let Some(servers) = parsed.get("mcpServers")
        && !servers.is_object()
    {
        state.errors.push(format!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        ));
        return;
    }
    match parsed.get("autoEnableCodemode") {
        Some(Value::Bool(value)) => state.auto_enable_codemode = Some(*value),
        Some(_) => state.errors.push(format!(
            "{}: autoEnableCodemode must be a boolean",
            path.display()
        )),
        None => {}
    }
    let Some(servers) = parsed.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (name, raw) in servers {
        let config = match validate_mcp_server_config(name, raw) {
            Ok(config) => config,
            Err(error) => {
                state.errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        // Names that differ only in `-` and `_` would share a namespace.
        if let Some(clash) = state
            .servers
            .iter()
            .find(|other| other.name != *name && mcp_namespace(&other.name) == mcp_namespace(name))
        {
            state.errors.push(format!(
                "{}: server \"{name}\" conflicts with \"{}\"",
                path.display(),
                clash.name
            ));
            continue;
        }
        if scope == McpScope::Project
            && config.url().is_some()
            && matches!(&config, McpServerConfig::Http(http) if http.auth.is_some())
        {
            state.errors.push(format!(
                "{}: server \"{name}\": auth is only allowed in the global mcp.json",
                path.display()
            ));
            continue;
        }
        state.servers.retain(|server| server.name != *name);
        state.servers.push(McpServerEntry {
            name: name.clone(),
            config,
            source: path.display().to_string(),
            scope: Some(scope),
        });
    }
}

/// `loadMcpConfig` (config.ts:91): global and (when trusted) project MCP
/// configuration. Disabled servers are included with `enabled: false`.
pub fn load_mcp_config(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> LoadedMcpConfig {
    let mut state = LoadedMcpConfig::default();
    read_config_file(&agent_dir.join("mcp.json"), McpScope::Global, &mut state);
    if project_trusted {
        read_config_file(
            &cwd.join(CONFIG_DIR_NAME).join("mcp.json"),
            McpScope::Project,
            &mut state,
        );
    }
    state
}

/// `McpServerConfigPatch` (config.ts:128): `/mcp` changes. Defaults
/// (`enabled: true`, `exposure: "codemode"`) remove the key.
#[derive(Debug, Clone, Copy, Default)]
pub struct McpServerConfigPatch {
    pub enabled: Option<bool>,
    pub exposure: Option<McpExposure>,
}

/// `updateMcpServerConfig` (config.ts:135).
pub fn update_mcp_server_config(
    path: &Path,
    name: &str,
    patch: McpServerConfigPatch,
) -> Result<(), String> {
    edit_mcp_servers(path, |servers| {
        let Some(server) = servers.get_mut(name).and_then(Value::as_object_mut) else {
            return Err(format!(
                "{} does not define MCP server \"{name}\"",
                path.display()
            ));
        };
        if let Some(enabled) = patch.enabled {
            if enabled {
                server.remove("enabled");
            } else {
                server.insert("enabled".to_owned(), Value::Bool(false));
            }
        }
        if let Some(exposure) = patch.exposure {
            if exposure == McpExposure::Codemode {
                server.remove("exposure");
            } else {
                server.insert(
                    "exposure".to_owned(),
                    Value::String(exposure.as_str().to_owned()),
                );
            }
        }
        Ok(())
    })
}

/// `addMcpServerConfig` (config.ts:158): true when an entry was replaced.
pub fn add_mcp_server_config(
    path: &Path,
    name: &str,
    config: &McpServerConfig,
) -> Result<bool, String> {
    let mut replaced = false;
    edit_mcp_servers(path, |servers| {
        replaced = servers.contains_key(name);
        servers.insert(name.to_owned(), config.to_json());
        Ok(())
    })?;
    Ok(replaced)
}

/// `removeMcpServerConfig` (config.ts:173): false when the file does not
/// define it.
pub fn remove_mcp_server_config(path: &Path, name: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    edit_mcp_servers(path, |servers| {
        removed = servers.remove(name).is_some();
        Ok(())
    })?;
    Ok(removed)
}

/// `editMcpServers` (config.ts:185): read an `mcp.json` (empty when
/// missing), let `edit` change its `mcpServers`, write it back with its
/// indentation. Other content is kept.
fn edit_mcp_servers(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<(), String>,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path).ok();
    let parsed: Value = match &text {
        Some(text) => {
            serde_json::from_str(text).map_err(|error| format!("{}: {error}", path.display()))?
        }
        None => Value::Object(Map::new()),
    };
    let mut parsed = match parsed {
        Value::Object(map) => map,
        _ => {
            return Err(format!(
                "{}: expected an object with an \"mcpServers\" object",
                path.display()
            ));
        }
    };
    if let Some(servers) = parsed.get("mcpServers")
        && !servers.is_object()
    {
        return Err(format!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        ));
    }
    let mut servers = parsed
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    edit(&mut servers)?;
    parsed.insert("mcpServers".to_owned(), Value::Object(servers));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let indent = text
        .as_deref()
        .map(detect_indent)
        .unwrap_or_else(|| "  ".to_owned());
    let mut body = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut body, formatter);
    serde::Serialize::serialize(&Value::Object(parsed), &mut serializer)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    body.push(b'\n');
    std::fs::write(path, body).map_err(|error| format!("{}: {error}", path.display()))
}

/// First indentation unit in the file (`/^([ \t]+)\S/m`).
fn detect_indent(text: &str) -> String {
    for line in text.lines() {
        let indent: String = line
            .chars()
            .take_while(|character| *character == ' ' || *character == '\t')
            .collect();
        if !indent.is_empty() && line.chars().count() > indent.chars().count() {
            return indent;
        }
    }
    "  ".to_owned()
}

/// `RegisteredMcpServer` (mcp-servers.ts:283): a server an extension
/// registered with `pi.registerMcpServer()`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredMcpServer {
    pub name: String,
    pub config: McpServerConfig,
    pub extension_path: String,
}

/// `McpServerRegistry` (mcp-servers.ts:290): servers registered by the
/// extensions of one runtime, in registration order.
#[derive(Default)]
pub struct McpServerRegistry {
    servers: Vec<RegisteredMcpServer>,
    change_listener: Option<Box<dyn Fn() + Send + Sync>>,
}

impl McpServerRegistry {
    /// `register` (mcp-servers.ts:296): register or replace, then notify.
    pub fn register(&mut self, server: RegisteredMcpServer) {
        match self
            .servers
            .iter_mut()
            .find(|existing| existing.name == server.name)
        {
            Some(existing) => *existing = server,
            None => self.servers.push(server),
        }
        self.notify();
    }

    /// `unregister` (mcp-servers.ts:302): remove a server registered by
    /// `extension_path`; other extensions' servers are left alone.
    pub fn unregister(&mut self, name: &str, extension_path: &str) {
        let before = self.servers.len();
        self.servers
            .retain(|server| !(server.name == name && server.extension_path == extension_path));
        if self.servers.len() != before {
            self.notify();
        }
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredMcpServer> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// `list` (mcp-servers.ts:314): copies in registration order.
    pub fn list(&self) -> Vec<RegisteredMcpServer> {
        self.servers.clone()
    }

    /// `setChangeListener` (mcp-servers.ts:320).
    pub fn set_change_listener(&mut self, listener: Option<Box<dyn Fn() + Send + Sync>>) {
        self.change_listener = listener;
    }

    fn notify(&self) {
        if let Some(listener) = &self.change_listener {
            listener();
        }
    }

    /// The `RegisteredMcpServer[]` JSON the `mcp_servers_change` event and
    /// `getMcpServers()` use.
    pub fn to_json(&self) -> Vec<Value> {
        self.list()
            .into_iter()
            .map(|server| {
                json!({
                    "name": server.name,
                    "config": server.config.to_json(),
                    "extensionPath": server.extension_path,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_http_and_stdio_shapes() {
        let http = validate_mcp_server_config(
            "docs",
            &json!({"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer x"}}),
        )
        .unwrap();
        assert_eq!(http.exposure(), McpExposure::Codemode);
        assert!(http.enabled());
        assert_eq!(http.timeout_ms(), 60_000);

        let stdio = validate_mcp_server_config(
            "fs",
            &json!({"command": "npx", "args": ["-y", "server"], "env": {"A": "B"}}),
        )
        .unwrap();
        assert!(matches!(stdio, McpServerConfig::Stdio(_)));

        for (name, raw, needle) in [
            ("bad name!", json!({"command": "x"}), "invalid server name"),
            (
                "sse",
                json!({"type": "sse", "url": "https://x"}),
                "legacy SSE",
            ),
            ("t", json!({"command": "x", "args": "nope"}), "args must be"),
            (
                "t",
                json!({"command": "x", "timeout": 0}),
                "timeout must be",
            ),
            ("t", json!({}), "needs either"),
            ("t", json!({"url": "ftp://x"}), "http or https"),
        ] {
            let error = validate_mcp_server_config(name, &raw).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn exposure_alias_and_tool_overrides() {
        let config = validate_mcp_server_config(
            "s",
            &json!({
                "url": "https://x",
                "exposure": "codemode-deferred",
                "toolExposure": {"write_*": "direct", "read": "hidden"},
            }),
        )
        .unwrap();
        assert_eq!(config.exposure(), McpExposure::Codemode);
        assert_eq!(get_mcp_tool_exposure(&config, "read"), McpExposure::Hidden);
        assert_eq!(
            get_mcp_tool_exposure(&config, "write_file"),
            McpExposure::Direct
        );
        assert_eq!(
            get_mcp_tool_exposure(&config, "other"),
            McpExposure::Codemode
        );
        // An exact name wins over a pattern.
        let config = validate_mcp_server_config(
            "s",
            &json!({"url": "https://x", "toolExposure": {"write_*": "direct", "write_x": "hidden"}}),
        )
        .unwrap();
        assert_eq!(
            get_mcp_tool_exposure(&config, "write_x"),
            McpExposure::Hidden
        );
    }

    #[test]
    fn auth_and_metadata_url_constraints() {
        let error = validate_mcp_server_config(
            "s",
            &json!({"url": "http://example.com/mcp", "auth": {"provider": "radius"}}),
        )
        .unwrap_err();
        assert!(error.contains("auth requires an https URL"), "{error}");
        validate_mcp_server_config(
            "s",
            &json!({"url": "http://127.0.0.1:8080/mcp", "auth": {"provider": "radius"}}),
        )
        .unwrap();
        let error = validate_mcp_server_config(
            "s",
            &json!({"url": "https://x", "oauth": {"authServerMetadataUrl": "http://as.example/meta"}}),
        )
        .unwrap_err();
        assert!(error.contains("authServerMetadataUrl"), "{error}");
        validate_mcp_server_config(
            "s",
            &json!({"url": "https://x", "oauth": {"authServerMetadataUrl": "http://localhost:1/meta"}}),
        )
        .unwrap();
    }

    #[test]
    fn namespace_replaces_dashes() {
        assert_eq!(mcp_namespace("dev-radius"), "mcp__dev_radius");
        assert_eq!(mcp_namespace("docs"), "mcp__docs");
    }

    #[test]
    fn loads_global_then_project_with_override_and_conflicts() {
        let dir = tempdir();
        let agent = dir.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("mcp.json"),
            r#"{"mcpServers": {"a": {"command": "x"}, "b-c": {"url": "https://x"}, "auto": {"command": "y", "enabled": false}}, "autoEnableCodemode": false}"#,
        )
        .unwrap();
        let cwd = dir.join("project");
        std::fs::create_dir_all(cwd.join(CONFIG_DIR_NAME)).unwrap();
        std::fs::write(
            cwd.join(CONFIG_DIR_NAME).join("mcp.json"),
            r#"{"mcpServers": {"a": {"url": "https://project.example/mcp"}, "b_c": {"url": "https://clash"}, "auth": {"url": "http://localhost:9/mcp", "auth": {"provider": "p"}}}}"#,
        )
        .unwrap();
        let loaded = load_mcp_config(&agent, &cwd, true);
        assert_eq!(loaded.auto_enable_codemode, Some(false));
        assert!(
            loaded
                .errors
                .iter()
                .any(|error| error.contains("conflicts")),
            "{:?}",
            loaded.errors
        );
        assert!(
            loaded
                .errors
                .iter()
                .any(|error| error.contains("auth is only allowed")),
            "{:?}",
            loaded.errors
        );
        let a = loaded
            .servers
            .iter()
            .find(|server| server.name == "a")
            .unwrap();
        assert_eq!(a.config.url(), Some("https://project.example/mcp"));
        assert_eq!(a.scope, Some(McpScope::Project));
        // The untrusted project is ignored entirely.
        let untrusted = load_mcp_config(&agent, &cwd, false);
        assert_eq!(
            untrusted
                .servers
                .iter()
                .find(|server| server.name == "a")
                .unwrap()
                .config
                .url(),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edits_preserve_other_content_and_indent() {
        let dir = tempdir();
        let path = dir.join("mcp.json");
        std::fs::write(&path, "{\n    \"other\": 1,\n    \"mcpServers\": {\n        \"s\": {\n            \"command\": \"x\"\n        }\n    }\n}\n").unwrap();
        update_mcp_server_config(
            &path,
            "s",
            McpServerConfigPatch {
                enabled: Some(false),
                exposure: Some(McpExposure::Deferred),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"other\": 1"));
        assert!(text.contains("\"enabled\": false"));
        assert!(text.contains("\"exposure\": \"deferred\""));
        assert!(text.contains("\n    \"other\""), "indent preserved: {text}");
        assert!(remove_mcp_server_config(&path, "s").unwrap());
        assert!(!remove_mcp_server_config(&path, "s").unwrap());
        let added = add_mcp_server_config(
            &path,
            "new",
            &validate_mcp_server_config("new", &json!({"command": "x"})).unwrap(),
        )
        .unwrap();
        assert!(!added);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-mcp-config-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
