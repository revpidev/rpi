//! The built-in `mcp` extension (port of
//! `packages/coding-agent/src/extensions/mcp/index.ts` and `ui.ts` @
//! a13d35a74): connects `mcp.json` and extension-registered servers when a
//! session starts, registers their tools, renders the `mcp_servers` system
//! prompt section and drives `/mcp`.
//!
//! `/mcp` uses the generic select/input UI primitives (the upstream
//! full-screen manager collapses into menu steps here); sign-in URLs are
//! emitted as OSC 8 hyperlinks in TUI mode.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_ext_host::api::{ExtensionApi, ExtensionCommandContext, ExtensionContext, NotifyType};
use rpi_ext_host::types::{EVENT_MCP_SERVERS_CHANGE, ToolDefinition, ToolExposure, ToolNamespace};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::config::{
    McpExposure, McpScope, McpServerConfig, McpServerConfigPatch, McpServerEntry, load_mcp_config,
    mcp_namespace, validate_mcp_server_config,
};
use super::log::McpServerLog;
use super::oauth::{McpOAuthCredentialStore, McpSignInPrompt, sign_in_mcp_server};
use super::resources::{
    CreateResourceToolOptions, create_mcp_resource_tool_definitions, is_resource_tool,
};
use super::runtime::{
    McpServerConnection, McpServerConnectionHandle, McpServerConnectionOptions,
    McpTransportFactory, ServerState, create_default_transport, uses_oauth,
};
use super::tools::{CreateMcpToolOptions, create_mcp_tool_definition, create_mcp_tool_name};

/// `MCP_SERVERS_SECTION` (index.ts:149).
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";
/// `MAX_SERVER_DESCRIPTION_CHARS` (index.ts:152).
const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;
/// `MAX_SERVERS_SECTION_CHARS` (index.ts:157).
pub const MAX_SERVERS_SECTION_CHARS: usize = 4096;
/// `DEFAULT_STARTUP_WAIT_MS` (index.ts:75).
const DEFAULT_STARTUP_WAIT_MS: u64 = 10_000;
/// `MCP_USAGE` (index.ts:388).
const MCP_USAGE: &str =
    "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";

fn error_message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or_default()
}

/// `hasDirectTools` (index.ts:129).
pub fn has_direct_tools(entry: &McpServerEntry) -> bool {
    let mut exposures: HashSet<McpExposure> = HashSet::new();
    exposures.insert(entry.config.exposure());
    if let Some(overrides) = &entry.config.common().tool_exposure {
        for value in overrides.values() {
            if let Some(exposure) = value.as_str().and_then(McpExposure::parse) {
                exposures.insert(exposure);
            }
        }
    }
    exposures.contains(&McpExposure::Direct)
}

/// `hasIndirectTools` (index.ts:135).
pub fn has_indirect_tools(entry: &McpServerEntry) -> bool {
    let mut exposures: HashSet<McpExposure> = HashSet::new();
    exposures.insert(entry.config.exposure());
    if let Some(overrides) = &entry.config.common().tool_exposure {
        for value in overrides.values() {
            if let Some(exposure) = value.as_str().and_then(McpExposure::parse) {
                exposures.insert(exposure);
            }
        }
    }
    exposures.contains(&McpExposure::Codemode) || exposures.contains(&McpExposure::Deferred)
}

/// `serversSectionIntro` (index.ts:165).
fn servers_section_intro(reaches_codemode: bool, reaches_tool_search: bool) -> String {
    let mut intro = "MCP servers whose tools are not declared to you.".to_owned();
    if reaches_codemode {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches_tool_search {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }
    intro
}

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return String::new();
    }
    let prefix: String = text.chars().take(max - 1).collect();
    format!("{}…", prefix.trim_end())
}

/// `McpServerListing` (index.ts:192).
pub struct McpServerListing {
    pub entry: McpServerEntry,
    pub instructions: Option<String>,
}

/// `serverSummary` (index.ts:199).
fn server_summary(listing: &McpServerListing) -> String {
    let text = listing
        .entry
        .config
        .common()
        .description
        .clone()
        .filter(|description| !description.trim().is_empty())
        .or_else(|| listing.instructions.clone())
        .unwrap_or_default();
    first_line(&text).trim().to_owned()
}

/// `renderServersSection` (index.ts:209): the `mcp_servers` section, or
/// `None` when there are no such servers.
pub fn render_servers_section(servers: &[McpServerListing]) -> Option<String> {
    let mut listed: Vec<&McpServerListing> = servers
        .iter()
        .filter(|listing| listing.entry.config.enabled() && has_indirect_tools(&listing.entry))
        .collect();
    listed.sort_by(|a, b| a.entry.name.cmp(&b.entry.name));
    if listed.is_empty() {
        return None;
    }
    let reaches: Vec<&str> = listed
        .iter()
        .map(|listing| {
            let codemode = {
                let mut exposures: HashSet<McpExposure> = HashSet::new();
                exposures.insert(listing.entry.config.exposure());
                if let Some(overrides) = &listing.entry.config.common().tool_exposure {
                    for value in overrides.values() {
                        if let Some(exposure) = value.as_str().and_then(McpExposure::parse) {
                            exposures.insert(exposure);
                        }
                    }
                }
                exposures.contains(&McpExposure::Codemode)
            };
            if codemode { "codemode" } else { "tool_search" }
        })
        .collect();
    let intro = servers_section_intro(
        reaches.contains(&"codemode"),
        reaches.contains(&"tool_search"),
    );
    let heads: Vec<String> = listed
        .iter()
        .zip(&reaches)
        .map(|(listing, reach)| format!("- {} ({reach})", mcp_namespace(&listing.entry.name)))
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        if count == 0 {
            Vec::new()
        } else {
            vec![format!(
                "- … {count} more server{}; find their tools with searchTools()",
                if count == 1 { "" } else { "s" }
            )]
        }
    };
    let size = |kept: usize| -> usize {
        let mut lines = vec![intro.clone()];
        lines.extend(heads.iter().take(kept).cloned());
        lines.extend(omitted(listed.len() - kept));
        lines.join("\n").chars().count()
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    let per_server = if kept == 0 {
        0
    } else {
        let available = MAX_SERVERS_SECTION_CHARS.saturating_sub(size(kept));
        (available / kept)
            .saturating_sub(2)
            .min(MAX_SERVER_DESCRIPTION_CHARS)
    };
    let mut lines = vec![intro];
    for (index, listing) in listed.iter().take(kept).enumerate() {
        let summary = if per_server > 0 {
            truncate_text(&server_summary(listing), per_server)
        } else {
            String::new()
        };
        if summary.is_empty() {
            lines.push(heads[index].clone());
        } else {
            lines.push(format!("{}: {summary}", heads[index]));
        }
    }
    lines.extend(omitted(listed.len() - kept));
    Some(lines.join("\n"))
}

/// `scriptNeedsServer` (index.ts:237): whether a codemode script names the
/// server's namespace, or searches/enumerates/describes tools.
pub fn script_needs_server(code: &str, server: &str) -> bool {
    for marker in [
        "searchTools",
        "describeNamespace",
        "describeTool",
        "ALL_TOOLS",
    ] {
        if code.contains(marker) {
            return true;
        }
    }
    code.contains(&mcp_namespace(server))
}

/// One configured server (`McpServer`, index.ts:84).
struct McpServer {
    entry: Mutex<McpServerEntry>,
    connection: Mutex<Option<Arc<McpServerConnection>>>,
    /// Registered servers: the config as registered, to detect
    /// re-registrations.
    registered_config: Option<String>,
    message: Mutex<Option<String>>,
    ready: Mutex<Option<watch::Receiver<bool>>>,
}

/// Tool bookkeeping shared by the registration paths.
#[derive(Default)]
struct ToolState {
    /// pi tool name → the `<server>\0<tool>` it was assigned to.
    tool_owners: HashMap<String, String>,
    /// Tool names currently offered by each server.
    server_tools: HashMap<String, HashSet<String>>,
    /// Last definition registered under each tool name.
    definitions: HashMap<String, ToolDefinition>,
    /// Exposure the resource tools were last registered with.
    resource_tools_exposure: Option<McpExposure>,
}

/// The extension's shared state.
pub struct McpBuiltinState {
    api: ExtensionApi,
    servers: Mutex<Vec<Arc<McpServer>>>,
    configured_entries: Mutex<Vec<McpServerEntry>>,
    config_errors: Mutex<Vec<String>>,
    overridden: Mutex<Vec<String>>,
    session_active: AtomicBool,
    auto_enable_codemode: AtomicBool,
    warned_unreachable: AtomicBool,
    waited_for_startup: AtomicBool,
    generation: AtomicU64,
    session_cwd: Mutex<String>,
    credentials: Mutex<Option<Arc<McpOAuthCredentialStore>>>,
    server_log: Mutex<Option<Arc<McpServerLog>>>,
    tools: Mutex<ToolState>,
    /// Servers that were waiting for a sign-in, with the token state they
    /// had then.
    tokens_at_sign_in: Mutex<HashMap<String, String>>,
    /// Deferred tool names a resumed/reloaded loadout still references but
    /// no server has registered yet (v1.0.0 `c662ec7e3`).
    pending_tools: Mutex<HashSet<String>>,
    /// Resolves `auth.provider` tokens (`/login` credentials).
    model_runtime: Arc<crate::core::model_runtime::ModelRuntime>,
}

impl McpBuiltinState {
    fn new(
        api: ExtensionApi,
        model_runtime: Arc<crate::core::model_runtime::ModelRuntime>,
    ) -> Self {
        Self {
            api,
            servers: Mutex::new(Vec::new()),
            configured_entries: Mutex::new(Vec::new()),
            config_errors: Mutex::new(Vec::new()),
            overridden: Mutex::new(Vec::new()),
            session_active: AtomicBool::new(false),
            auto_enable_codemode: AtomicBool::new(true),
            warned_unreachable: AtomicBool::new(false),
            waited_for_startup: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            session_cwd: Mutex::new(String::new()),
            credentials: Mutex::new(None),
            server_log: Mutex::new(None),
            tools: Mutex::new(ToolState::default()),
            tokens_at_sign_in: Mutex::new(HashMap::new()),
            pending_tools: Mutex::new(HashSet::new()),
            model_runtime,
        }
    }

    fn find_server(&self, name: &str) -> Option<Arc<McpServer>> {
        lock(&self.servers)
            .iter()
            .find(|server| lock(&server.entry).name == name)
            .cloned()
    }

    fn credentials(&self) -> Arc<McpOAuthCredentialStore> {
        let mut credentials = lock(&self.credentials);
        credentials
            .get_or_insert_with(|| {
                Arc::new(McpOAuthCredentialStore::new(&crate::config::get_agent_dir()))
            })
            .clone()
    }

    fn server_log(&self) -> Arc<McpServerLog> {
        let mut log = lock(&self.server_log);
        log.get_or_insert_with(|| {
            Arc::new(McpServerLog::new(
                crate::config::get_agent_dir().join("mcp.log"),
            ))
        })
        .clone()
    }

    fn transport_factory(&self) -> McpTransportFactory {
        Arc::new(create_default_transport)
    }

    /// Replaceable coexistence (FR-H, resource-loader.ts:116-146): when
    /// another extension already registered an `mcp` tool or `/mcp`
    /// command, the built-in stands down (no config read, no connections).
    fn replacer(&self) -> Option<String> {
        let own = self.api.extension().path.clone();
        let tool_replacer = self
            .api
            .get_all_tools()
            .unwrap_or_default()
            .into_iter()
            .find(|tool| {
                tool.get("name").and_then(Value::as_str) == Some("mcp")
                    && tool
                        .get("sourceInfo")
                        .and_then(|info| info.get("path"))
                        .and_then(Value::as_str)
                        .is_some_and(|path| path != own)
            });
        let command_replacer = self
            .api
            .get_commands()
            .unwrap_or_default()
            .into_iter()
            .find(|command| {
                command.get("name").and_then(Value::as_str) == Some("mcp")
                    && command
                        .get("sourceInfo")
                        .and_then(|info| info.get("path"))
                        .and_then(Value::as_str)
                        .is_some_and(|path| path != own)
            });
        tool_replacer
            .map(|tool| {
                tool.get("sourceInfo")
                    .and_then(|info| info.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or("<extension>")
                    .to_owned()
            })
            .or_else(|| {
                command_replacer.map(|command| {
                    command
                        .get("sourceInfo")
                        .and_then(|info| info.get("path"))
                        .and_then(Value::as_str)
                        .unwrap_or("<extension>")
                        .to_owned()
                })
            })
    }

    /// `ensureDiscoveryActive` (index.ts:456).
    fn ensure_discovery_active(&self, ctx: &ExtensionContext) {
        let mut exposures: HashSet<McpExposure> = HashSet::new();
        for entry in lock(&self.configured_entries).clone() {
            if entry.config.enabled() {
                exposures.insert(entry.config.exposure());
                if let Some(overrides) = &entry.config.common().tool_exposure {
                    for value in overrides.values() {
                        if let Some(exposure) = value.as_str().and_then(McpExposure::parse) {
                            exposures.insert(exposure);
                        }
                    }
                }
            }
        }
        for server in lock(&self.servers).clone() {
            let entry = lock(&server.entry).clone();
            if entry.config.enabled() {
                exposures.insert(entry.config.exposure());
                if let Some(overrides) = &entry.config.common().tool_exposure {
                    for value in overrides.values() {
                        if let Some(exposure) = value.as_str().and_then(McpExposure::parse) {
                            exposures.insert(exposure);
                        }
                    }
                }
            }
        }
        let needs_codemode = exposures.contains(&McpExposure::Codemode);
        let needs_tool_search = exposures.contains(&McpExposure::Deferred);
        if !needs_codemode && !needs_tool_search {
            return;
        }
        let tools = self.api.get_all_tools().unwrap_or_default();
        let has_codemode = tools.iter().any(|tool| {
            tool.get("name").and_then(Value::as_str)
                == Some(super::super::codemode::tool::CODEMODE_TOOL_NAME)
        });
        let has_tool_search = tools.iter().any(|tool| {
            tool.get("name").and_then(Value::as_str)
                == Some(super::super::tool_search::TOOL_SEARCH_TOOL_NAME)
        });
        let active = self.api.get_active_tools().unwrap_or_default();
        let mut activate: Vec<String> = Vec::new();
        if needs_codemode
            && has_codemode
            && self.auto_enable_codemode.load(Ordering::SeqCst)
            && !active.contains(&super::super::codemode::tool::CODEMODE_TOOL_NAME.to_owned())
        {
            activate.push(super::super::codemode::tool::CODEMODE_TOOL_NAME.to_owned());
        }
        if needs_tool_search
            && has_tool_search
            && !active.contains(&super::super::tool_search::TOOL_SEARCH_TOOL_NAME.to_owned())
        {
            activate.push(super::super::tool_search::TOOL_SEARCH_TOOL_NAME.to_owned());
        }
        if !activate.is_empty() {
            let mut all = active.clone();
            all.extend(activate);
            let _ = self.api.set_active_tools(all);
        }
        let reachable = {
            let mut all = active;
            all.extend([
                super::super::codemode::tool::CODEMODE_TOOL_NAME.to_owned(),
                super::super::tool_search::TOOL_SEARCH_TOOL_NAME.to_owned(),
            ]);
            all
        };
        if has_codemode
            && reachable
                .iter()
                .any(|name| name == super::super::codemode::tool::CODEMODE_TOOL_NAME)
        {
            return;
        }
        if has_tool_search
            && reachable
                .iter()
                .any(|name| name == super::super::tool_search::TOOL_SEARCH_TOOL_NAME)
        {
            return;
        }
        if self.warned_unreachable.swap(true, Ordering::SeqCst) {
            return;
        }
        let reason = if needs_codemode
            && has_codemode
            && !self.auto_enable_codemode.load(Ordering::SeqCst)
        {
            " (autoEnableCodemode is false)"
        } else {
            ""
        };
        if let Ok(ui) = ctx.ui() {
            ui.notify(
                &format!(
                    "MCP tools are only reachable from the codemode or tool_search tool, but neither is active{reason}; they cannot be called."
                ),
                NotifyType::Warning,
            );
        }
    }

    /// `registerTools` (index.ts:284).
    fn register_tools(self: &Arc<Self>, connection: &Arc<McpServerConnection>) {
        let server_name = connection.name().to_owned();
        let entry = self
            .find_server(&server_name)
            .map(|server| lock(&server.entry).clone())
            .unwrap_or_else(|| connection.entry().clone());
        let description = entry
            .config
            .common()
            .description
            .clone()
            .filter(|description| !description.trim().is_empty());
        let namespace = ToolNamespace {
            name: mcp_namespace(&server_name),
            description,
            instructions: connection.instructions(),
        };
        let tools = connection.tools();
        let previous = lock(&self.tools)
            .server_tools
            .get(&server_name)
            .cloned()
            .unwrap_or_default();
        let mut current: HashSet<String> = HashSet::new();
        // Like Codex, all tools whose names sanitize to the same name get the
        // hash suffix, so which one keeps the plain name does not depend on
        // the list order.
        let plain: Vec<String> = {
            let mut unique: Vec<String> = Vec::new();
            for tool in &tools {
                if !unique.contains(&tool.name) {
                    unique.push(tool.name.clone());
                }
            }
            unique
                .iter()
                .map(|tool| create_mcp_tool_name(&server_name, tool, |_| false))
                .collect()
        };
        let mut names_to_register: Vec<(usize, String)> = Vec::new();
        for (index, tool) in tools.iter().enumerate() {
            let owner = format!("{server_name}\u{0}{}", tool.name);
            let mut state = lock(&self.tools);
            let name = create_mcp_tool_name(&server_name, &tool.name, |candidate| {
                let existing = state.tool_owners.get(candidate);
                (existing.is_some_and(|existing| *existing != owner))
                    || current.contains(candidate)
                    || plain.iter().filter(|plain| *plain == candidate).count() > 1
            });
            state.tool_owners.insert(name.clone(), owner);
            drop(state);
            current.insert(name.clone());
            names_to_register.push((index, name));
        }
        for (_, name) in &names_to_register {
            let Some(index) = names_to_register
                .iter()
                .position(|(_, candidate)| candidate == name)
            else {
                continue;
            };
            let tool = tools[index].clone();
            let connection = connection.clone();
            let definition = create_mcp_tool_definition(CreateMcpToolOptions {
                server: server_name.clone(),
                tool: tool.clone(),
                name: name.clone(),
                exposure: super::config::get_mcp_tool_exposure(&entry.config, &tool.name),
                namespace: namespace.clone(),
                timeout_ms: connection.timeout_ms(),
                get_client: {
                    let connection = connection.clone();
                    Arc::new(move || {
                        let connection = connection.clone();
                        Box::pin(async move {
                            Ok(Arc::new(McpServerConnectionHandle(connection))
                                as Arc<dyn super::tools::McpToolCaller>)
                        })
                    })
                },
                readable_resources: {
                    let state = self.clone();
                    let connection = connection.clone();
                    Some(Arc::new(move || {
                        state
                            .resource_servers()
                            .iter()
                            .any(|server| Arc::ptr_eq(server, &connection))
                    }))
                },
            });
            lock(&self.tools)
                .definitions
                .insert(name.clone(), definition.clone());
            let _ = self.api.register_tool(definition);
        }
        {
            let mut state = lock(&self.tools);
            state
                .server_tools
                .insert(server_name.clone(), current.clone());
        }
        // Tools cannot be unregistered, so tools the server dropped are
        // re-registered as hidden.
        for name in previous {
            if !current.contains(&name)
                && let Some(definition) = lock(&self.tools).definitions.get(&name).cloned()
            {
                let hidden = ToolDefinition {
                    exposure: ToolExposure::Hidden,
                    ..definition
                };
                let _ = self.api.register_tool(hidden);
            }
        }
        self.sync_resource_tools();
    }

    /// `hideTools` (index.ts:337).
    fn hide_tools(self: &Arc<Self>, server: &str) {
        let names = lock(&self.tools)
            .server_tools
            .get(server)
            .cloned()
            .unwrap_or_default();
        for name in names {
            if let Some(definition) = lock(&self.tools).definitions.get(&name).cloned() {
                let hidden = ToolDefinition {
                    exposure: ToolExposure::Hidden,
                    ..definition
                };
                let _ = self.api.register_tool(hidden);
            }
        }
        lock(&self.tools)
            .server_tools
            .insert(server.to_owned(), HashSet::new());
        self.sync_resource_tools();
    }

    /// `serversWithResources` (index.ts:346).
    fn servers_with_resources(&self) -> Vec<Arc<McpServerConnection>> {
        lock(&self.servers)
            .iter()
            .filter(|server| {
                let entry = lock(&server.entry);
                server
                    .connection
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .is_some_and(|connection| connection.has_resources())
                    && entry.config.enabled()
                    && entry.config.exposure() != McpExposure::Hidden
            })
            .flat_map(|server| {
                server
                    .connection
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            })
            .collect()
    }

    fn resource_servers(&self) -> Vec<Arc<McpServerConnection>> {
        self.servers_with_resources()
    }

    /// `syncResourceTools` (index.ts:362): the widest exposure of the
    /// servers they reach.
    fn sync_resource_tools(self: &Arc<Self>) {
        let servers = self.servers_with_resources();
        let mut exposures: HashSet<McpExposure> = HashSet::new();
        for connection in &servers {
            let entry = connection.entry().clone();
            exposures.insert(entry.config.exposure());
        }
        let exposure = [
            McpExposure::Direct,
            McpExposure::Codemode,
            McpExposure::Deferred,
        ]
        .into_iter()
        .find(|candidate| exposures.contains(candidate));
        let next = exposure.unwrap_or(McpExposure::Hidden);
        let previous = lock(&self.tools).resource_tools_exposure;
        if previous == Some(next) || (previous.is_none() && next == McpExposure::Hidden) {
            return;
        }
        let was_direct = previous == Some(McpExposure::Direct);
        lock(&self.tools).resource_tools_exposure = Some(next);
        let state = Arc::new(ResourceStateHandle(self.clone()));
        let servers_closure: Arc<
            dyn Fn() -> Vec<Arc<dyn super::resources::McpResourceServer>> + Send + Sync,
        > = {
            Arc::new(move || {
                state
                    .resource_servers()
                    .into_iter()
                    .map(|connection| {
                        Arc::new(McpServerConnectionHandle(connection))
                            as Arc<dyn super::resources::McpResourceServer>
                    })
                    .collect()
            })
        };
        let definitions = create_mcp_resource_tool_definitions(CreateResourceToolOptions {
            exposure: next,
            servers: servers_closure,
        });
        for definition in &definitions {
            let _ = self.api.register_tool(definition.clone());
        }
        if was_direct {
            let names: HashSet<String> = definitions
                .into_iter()
                .map(|definition| definition.name)
                .collect();
            let active: Vec<String> = self
                .api
                .get_active_tools()
                .unwrap_or_default()
                .into_iter()
                .filter(|name| !names.contains(name))
                .collect();
            let _ = self.api.set_active_tools(active);
        }
    }

    /// `onConnectionChange` (index.ts:521).
    fn on_connection_change(self: &Arc<Self>, connection: &Arc<McpServerConnection>) {
        let name = connection.name().to_owned();
        if connection.state() != ServerState::NeedsAuth {
            lock(&self.tokens_at_sign_in).remove(&name);
        } else if !lock(&self.tokens_at_sign_in).contains_key(&name) {
            let tokens = self
                .stored_tokens(connection)
                .unwrap_or_else(|| "null".to_owned());
            lock(&self.tokens_at_sign_in).insert(name, tokens);
        }
    }

    /// `storedTokens` (index.ts:518).
    fn stored_tokens(&self, connection: &Arc<McpServerConnection>) -> Option<String> {
        let url = connection.oauth_url()?;
        Some(
            serde_json::to_string(&self.credentials().tokens(connection.name(), &url))
                .unwrap_or_else(|_| "null".to_owned()),
        )
    }

    /// `reconnectSignedIn` (index.ts:529).
    async fn reconnect_signed_in(self: &Arc<Self>, ctx: &ExtensionContext) {
        let candidates: Vec<String> = {
            let tokens = lock(&self.tokens_at_sign_in);
            tokens
                .iter()
                .filter(|(name, old)| {
                    self.find_server(name)
                        .and_then(|server| {
                            server
                                .connection
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .clone()
                        })
                        .and_then(|connection| self.stored_tokens(&connection))
                        .is_some_and(|current| Some(current) != Some((*old).clone()))
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        if candidates.is_empty() {
            return;
        }
        for name in &candidates {
            lock(&self.tokens_at_sign_in).remove(name);
        }
        let mut reconnects = Vec::new();
        for name in &candidates {
            if let Some(server) = self.find_server(name)
                && let Some(connection) = server
                    .connection
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            {
                reconnects.push(tokio::spawn(async move {
                    let _ = connection.reconnect().await;
                }));
            }
        }
        for reconnect in reconnects {
            let _ = reconnect.await;
        }
        self.ensure_discovery_active(ctx);
    }

    /// `createConnection` (index.ts:540).
    async fn create_connection(
        self: &Arc<Self>,
        server: Arc<McpServer>,
    ) -> Result<Arc<McpServerConnection>, String> {
        let entry = lock(&server.entry).clone();
        let cwd = lock(&self.session_cwd).clone();
        let state = self.clone();
        let on_tools = {
            let state = state.clone();
            Arc::new(move |connection: &McpServerConnection| {
                // The callback receives `&McpServerConnection`; the
                // registration needs the owning `Arc`, which the server slot
                // holds.
                if let Some(server) = state.find_server(connection.name())
                    && let Some(connection) = server
                        .connection
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone()
                {
                    state.register_tools(&connection);
                }
            })
        };
        let on_change = {
            let state = state.clone();
            Some(Arc::new(move |connection: &McpServerConnection| {
                if let Some(server) = state.find_server(connection.name())
                    && let Some(connection) = server
                        .connection
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone()
                {
                    state.on_connection_change(&connection);
                }
            })
                as Arc<dyn Fn(&McpServerConnection) + Send + Sync>)
        };
        let provider_token = {
            let runtime = self.model_runtime.clone();
            Some(Arc::new(move |provider: String| {
                let runtime = runtime.clone();
                Box::pin(async move {
                    runtime
                        .get_provider_auth(&provider, None)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|auth| auth.auth.api_key)
                }) as futures::future::BoxFuture<'static, Option<String>>
            }) as super::runtime::ProviderTokenFn)
        };
        let connection = Arc::new(McpServerConnection::new(McpServerConnectionOptions {
            entry,
            cwd,
            create_transport: self.transport_factory(),
            credentials: self.credentials(),
            provider_token,
            on_tools,
            on_change,
            log: Some(self.server_log()),
        }));
        *server
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(connection.clone());
        Ok(connection)
    }

    /// `startConnection` (index.ts:560).
    fn start_connection(
        self: &Arc<Self>,
        server: Arc<McpServer>,
        generation: u64,
    ) -> watch::Receiver<bool> {
        let (sender, receiver) = watch::channel(false);
        let state = self.clone();
        tokio::spawn(async move {
            let is_current = {
                let state = state.clone();
                move || state.generation.load(Ordering::SeqCst) == generation
            };
            if !is_current() {
                let _ = sender.send(true);
                return;
            }
            if let Ok(connection) = state.create_connection(server).await
                && is_current()
            {
                let _ = connection.get_client().await;
            }
            let _ = sender.send(true);
        });
        receiver
    }

    /// `waitForServers` (index.ts:576).
    async fn wait_for_servers(
        &self,
        waiting: Vec<watch::Receiver<bool>>,
        signal: Option<CancellationToken>,
    ) {
        let mut receivers = waiting;
        if receivers.is_empty() {
            return;
        }
        let wait = async {
            for receiver in &mut receivers {
                let mut receiver = receiver.clone();
                if *receiver.borrow() {
                    continue;
                }
                let _ = receiver.wait_for(|ready| *ready).await;
            }
        };
        match signal {
            Some(signal) => {
                tokio::select! {
                    () = wait => {}
                    () = signal.cancelled() => {}
                }
            }
            None => wait.await,
        }
    }

    fn pending_servers(&self) -> Vec<(Arc<McpServer>, watch::Receiver<bool>, bool)> {
        lock(&self.servers)
            .iter()
            .filter(|server| {
                let entry = lock(&server.entry);
                entry.config.enabled()
            })
            .flat_map(|server| {
                let entry = lock(&server.entry).clone();
                let connection = server
                    .connection
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let connected = connection
                    .as_ref()
                    .is_some_and(|connection| connection.state() == ServerState::Connected);
                let ready = server
                    .ready
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                match ready {
                    Some(ready) if !connected => {
                        Some((server.clone(), ready, has_direct_tools(&entry)))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// `reportProblems` (index.ts:600).
    fn report_problems(&self, ctx: &ExtensionContext, only: Option<&[Arc<McpServer>]>) {
        let mut lines: Vec<String> = Vec::new();
        if only.is_none() {
            lines.extend(
                lock(&self.config_errors)
                    .iter()
                    .map(|error| format!("config: {error}")),
            );
        }
        match only {
            Some(only) => {
                for server in only {
                    let entry = lock(&server.entry);
                    if entry.config.enabled()
                        && let Some(message) = describe_state(server)
                    {
                        lines.push(format!("{}: {message}", entry.name));
                    }
                }
            }
            None => {
                for server in lock(&self.servers).clone() {
                    let entry = lock(&server.entry);
                    if entry.config.enabled()
                        && let Some(message) = describe_state(&server)
                    {
                        lines.push(format!("{}: {message}", entry.name));
                    }
                }
            }
        }
        if lines.is_empty() {
            return;
        }
        if let Ok(ui) = ctx.ui() {
            let body = lines
                .iter()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            ui.notify(
                &format!("MCP servers need attention:\n{body}\nRun /mcp to fix."),
                NotifyType::Warning,
            );
        }
    }

    /// `setEnabled` (index.ts:647): returns an error message when the config
    /// could not be saved.
    async fn set_enabled(
        self: &Arc<Self>,
        ctx: &ExtensionContext,
        name: &str,
        enabled: bool,
    ) -> Option<String> {
        let server = self.find_server(name)?;
        if let Some(failure) = self.save_config(
            &server,
            McpServerConfigPatch {
                enabled: Some(enabled),
                exposure: None,
            },
        ) {
            return Some(failure);
        }
        if !enabled {
            let connection = server
                .connection
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            self.hide_tools(name);
            if let Some(connection) = connection {
                connection.close().await;
            }
            return None;
        }
        let generation = self.generation.load(Ordering::SeqCst);
        let server_for_ready = server.clone();
        let ready = self.start_connection(server, generation);
        *lock_server_ready(&server_for_ready) = Some(ready);
        self.ensure_discovery_active(ctx);
        None
    }

    /// `saveConfig` (index.ts:625): changes to registered servers apply to
    /// the current session only.
    fn save_config(&self, server: &Arc<McpServer>, patch: McpServerConfigPatch) -> Option<String> {
        let entry = lock(&server.entry).clone();
        if entry.scope != Some(McpScope::Extension)
            && let Some(path) = entry.source.strip_prefix("file:")
        {
            if let Err(error) = super::config::update_mcp_server_config(
                std::path::Path::new(path),
                &entry.name,
                patch,
            ) {
                return Some(format!("Could not update {}: {error}", entry.source));
            }
        } else if entry.scope != Some(McpScope::Extension)
            && let Err(error) = super::config::update_mcp_server_config(
                std::path::Path::new(&entry.source),
                &entry.name,
                patch,
            )
        {
            return Some(format!("Could not update {}: {error}", entry.source));
        }
        let mut updated = entry;
        if let Some(enabled) = patch.enabled {
            updated.config.common_mut().enabled = Some(enabled);
        }
        if let Some(exposure) = patch.exposure {
            updated.config.common_mut().exposure = Some(exposure.as_str().to_owned());
        }
        *lock(&server.entry) = updated;
        None
    }

    /// `setExposure` (index.ts:663).
    fn set_exposure(self: &Arc<Self>, name: &str, exposure: McpExposure) -> Option<String> {
        let server = self.find_server(name)?;
        if let Some(failure) = self.save_config(
            &server,
            McpServerConfigPatch {
                enabled: None,
                exposure: Some(exposure),
            },
        ) {
            return Some(failure);
        }
        if let Some(connection) = server
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            && connection.state() == ServerState::Connected
        {
            self.register_tools(&connection);
        }
        self.sync_resource_tools();
        None
    }

    /// `formatStatus` (index.ts:807).
    fn format_status(&self) -> String {
        let servers = lock(&self.servers).clone();
        let errors = lock(&self.config_errors).clone();
        let overridden = lock(&self.overridden).clone();
        if servers.is_empty() && errors.is_empty() && overridden.is_empty() {
            return format!(
                "No MCP servers configured. Add them to {} or .rpi/mcp.json.",
                crate::config::get_agent_dir().join("mcp.json").display()
            );
        }
        let mut lines: Vec<String> = Vec::new();
        for server in servers {
            let entry = lock(&server.entry);
            let exposure = entry.config.exposure();
            let connection = server
                .connection
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if connection
                .as_ref()
                .is_some_and(|connection| connection.state() == ServerState::NeedsAuth)
            {
                lines.push(format!(
                    "{}: needs sign-in, run /mcp login {} ({})",
                    entry.name,
                    entry.name,
                    exposure.as_str()
                ));
                continue;
            }
            let tools = if connection
                .as_ref()
                .is_some_and(|connection| connection.state() == ServerState::Connected)
            {
                format!(
                    ", {} tools",
                    connection
                        .as_ref()
                        .map(|connection| connection.tools().len())
                        .unwrap_or(0)
                )
            } else {
                String::new()
            };
            let state = if !entry.config.enabled() {
                "disabled".to_owned()
            } else {
                match connection.as_ref().map(|connection| connection.state()) {
                    Some(ServerState::Disconnected) => {
                        "disconnected, reconnects on next call".to_owned()
                    }
                    Some(state) => state.as_str().to_owned(),
                    None => "starting".to_owned(),
                }
            };
            let error = connection
                .as_ref()
                .filter(|connection| connection.state() != ServerState::Connected)
                .and_then(|connection| connection.error())
                .map(|error| format!("\n    {}", error.replace('\n', "\n    ")))
                .unwrap_or_default();
            lines.push(format!(
                "{}: {state}{tools} ({}){error}",
                entry.name,
                exposure.as_str()
            ));
            lines.push(format!("  {}", describe_transport(&entry)));
        }
        for error in errors {
            lines.push(format!("config error: {error}"));
        }
        for line in overridden {
            lines.push(format!("overridden: {line}"));
        }
        lines.join("\n")
    }

    /// `signIn` (index.ts:636).
    async fn sign_in(
        &self,
        server: &Arc<McpServer>,
        prompt: &dyn McpSignInPrompt,
    ) -> Option<String> {
        let connection = server
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(connection) = connection else {
            return Some(format!(
                "MCP server \"{}\" does not use OAuth.",
                lock(&server.entry).name
            ));
        };
        let Some(url) = connection.oauth_url() else {
            return Some(format!(
                "MCP server \"{}\" does not use OAuth.",
                lock(&server.entry).name
            ));
        };
        let store = self.credentials().for_server(connection.name(), &url);
        let result = sign_in_mcp_server(super::oauth::SignInOptions {
            server_url: url,
            store: &store,
            settings: connection.oauth_settings(),
            challenge: connection.challenge(),
            prompt,
        })
        .await;
        if let Err(error) = result {
            if error.contains("Sign-in cancelled") {
                return Some("Sign-in cancelled.".to_owned());
            }
            return Some(format!("Sign-in failed: {error}"));
        }
        connection.set_challenge(None);
        if let Err(error) = connection.reconnect().await {
            return Some(format!("Signed in, but {error}"));
        }
        None
    }

    /// `signOut` (index.ts:664).
    async fn sign_out(&self, server: &Arc<McpServer>) -> bool {
        let connection = server
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(connection) = connection else {
            return false;
        };
        let Some(url) = connection.oauth_url() else {
            return false;
        };
        let removed = self.credentials().remove(connection.name(), &url);
        connection.sign_out().await;
        removed
    }

    fn pick_server(&self, name: &str) -> Option<Arc<McpServer>> {
        self.find_server(name)
    }

    /// Latest server list for the manager.
    fn server_snapshot(&self) -> Vec<Arc<McpServer>> {
        let mut servers = lock(&self.servers).clone();
        servers.sort_by(|a, b| {
            let rank = |server: &Arc<McpServer>| match server
                .connection
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .map(|connection| connection.state())
            {
                Some(ServerState::NeedsAuth) => 0,
                Some(ServerState::Failed) => 1,
                Some(ServerState::Disconnected) => 2,
                Some(ServerState::Connecting) => 3,
                Some(ServerState::Connected) => 4,
                _ => 5,
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| lock(&a.entry).name.cmp(&lock(&b.entry).name))
        });
        servers
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn lock_server_ready(
    server: &McpServer,
) -> std::sync::MutexGuard<'_, Option<watch::Receiver<bool>>> {
    server
        .ready
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// `describeState` (index.ts:248): short state for lists and the startup
/// report.
fn describe_state(server: &McpServer) -> Option<String> {
    let entry = lock(&server.entry);
    if !entry.config.enabled() {
        return None;
    }
    let connection = server
        .connection
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let Some(connection) = connection else {
        return Some("starting".to_owned());
    };
    Some(match connection.state() {
        ServerState::NeedsAuth => "needs sign-in".to_owned(),
        ServerState::Failed => format!(
            "failed: {}",
            first_line(connection.error().as_deref().unwrap_or("unknown error"))
        ),
        ServerState::Connected => {
            let tools = connection.tools().len();
            let resources = connection.resources().len();
            let resource_count = if resources > 0 {
                format!(
                    " · {resources} resource{}",
                    if resources == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            };
            format!(
                "connected · {tools} tool{}{resource_count}",
                if tools == 1 { "" } else { "s" }
            )
        }
        ServerState::Connecting => "connecting…".to_owned(),
        state => state.as_str().to_owned(),
    })
}

/// `describeTransport` (index.ts:380).
fn describe_transport(entry: &McpServerEntry) -> String {
    match &entry.config {
        McpServerConfig::Http(config) => config.url.clone(),
        McpServerConfig::Stdio(config) => {
            let mut parts = vec![config.command.clone()];
            parts.extend(config.args.clone().unwrap_or_default());
            parts.join(" ")
        }
    }
}

/// Resource-state handle for the resource-tool closures.
struct ResourceStateHandle(Arc<McpBuiltinState>);

impl ResourceStateHandle {
    fn resource_servers(&self) -> Vec<Arc<McpServerConnection>> {
        self.0.resource_servers()
    }
}

/// Register the built-in extension (`createMcpExtension`, index.ts:386).
pub fn create_mcp_extension(
    api: ExtensionApi,
    model_runtime: Arc<crate::core::model_runtime::ModelRuntime>,
) -> Result<(), String> {
    let state = Arc::new(McpBuiltinState::new(api.clone(), model_runtime));
    state.clone().install(&api)
}

impl McpBuiltinState {
    fn install(self: Arc<Self>, api: &ExtensionApi) -> Result<(), String> {
        {
            let state = self.clone();
            api.on(
                "session_start",
                Arc::new(move |_event, ctx| {
                    state.clone().on_session_start(ctx);
                    Box::pin(async { Ok(Value::Null) })
                }),
            )
            .map_err(error_message)?;
        }
        {
            let state = self.clone();
            api.on(
                "before_agent_start",
                Arc::new(move |event, ctx| {
                    let state = state.clone();
                    Box::pin(async move { state.clone().on_before_agent_start(event, &ctx).await })
                }),
            )
            .map_err(error_message)?;
        }
        {
            let state = self.clone();
            api.on(
                "tool_call",
                Arc::new(move |event, ctx| {
                    let state = state.clone();
                    Box::pin(async move { state.clone().on_tool_call(event, &ctx).await })
                }),
            )
            .map_err(error_message)?;
        }
        {
            let state = self.clone();
            api.on(
                "turn_start",
                Arc::new(move |_event, ctx| {
                    let state = state.clone();
                    Box::pin(async move {
                        state.clone().reconnect_signed_in(&ctx).await;
                        Ok(Value::Null)
                    })
                }),
            )
            .map_err(error_message)?;
        }
        {
            let state = self.clone();
            api.on(
                EVENT_MCP_SERVERS_CHANGE,
                Arc::new(move |_event, ctx| {
                    let state = state.clone();
                    Box::pin(async move { state.clone().on_mcp_servers_change(&ctx).await })
                }),
            )
            .map_err(error_message)?;
        }
        {
            let state = self.clone();
            api.on(
                "session_shutdown",
                Arc::new(move |_event, _ctx| {
                    let state = state.clone();
                    Box::pin(async move { state.clone().on_session_shutdown().await })
                }),
            )
            .map_err(error_message)?;
        }
        {
            let state = self.clone();
            api.register_command_with_completions(
                "mcp",
                Some("Manage MCP servers: sign in, reconnect, enable or disable, and change exposure".to_owned()),
                Some(Arc::new(move |prefix| {
                    let state = state.clone();
                    Box::pin(async move { Ok(state.argument_completions(&prefix)) })
                })),
                Arc::new(move |args, ctx| {
                    let state = self.clone();
                    Box::pin(async move { state.run_command(args, ctx).await })
                }),
            )
            .map_err(error_message)?;
        }
        Ok(())
    }

    fn argument_completions(&self, prefix: &str) -> Option<Value> {
        let parts: Vec<&str> = prefix.split_whitespace().collect();
        let action = parts.first().copied().unwrap_or_default();
        if parts.len() > 2 {
            return None;
        }
        if parts.len() <= 1 {
            let items: Vec<Value> = ["login", "logout", "reconnect"]
                .into_iter()
                .filter(|item| item.starts_with(action))
                .map(|item| json!({"value": format!("{item} "), "label": item}))
                .collect();
            return if items.is_empty() {
                None
            } else {
                Some(Value::Array(items))
            };
        }
        if !matches!(action, "login" | "logout" | "reconnect") {
            return None;
        }
        let server_prefix = parts[1];
        let items: Vec<Value> = self
            .server_snapshot()
            .into_iter()
            .filter(|server| {
                let entry = lock(&server.entry);
                entry.name.starts_with(server_prefix)
                    && if action == "reconnect" {
                        server
                            .connection
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .is_some()
                    } else {
                        uses_oauth(&entry)
                    }
            })
            .map(|server| {
                let entry = lock(&server.entry);
                json!({
                    "value": format!("{action} {}", entry.name),
                    "label": entry.name,
                    "description": describe_state(&server).unwrap_or_default(),
                })
            })
            .collect();
        if items.is_empty() {
            None
        } else {
            Some(Value::Array(items))
        }
    }

    /// `session_start` handler (index.ts:935).
    fn on_session_start(self: &Arc<Self>, ctx: ExtensionContext) {
        if let Some(replacer) = self.replacer() {
            self.session_active.store(false, Ordering::SeqCst);
            *lock(&self.servers) = Vec::new();
            if let Ok(ui) = ctx.ui() {
                ui.notify(
                    &format!(
                        "MCP: extension \"{replacer}\" replaced the built-in MCP extension; its servers are not connected."
                    ),
                    NotifyType::Warning,
                );
            }
            return;
        }
        let cwd = ctx.cwd().unwrap_or_default().to_owned();
        let trusted = ctx.is_project_trusted().unwrap_or(false);
        let loaded = load_mcp_config(
            &crate::config::get_agent_dir(),
            std::path::Path::new(&cwd),
            trusted,
        );
        *lock(&self.config_errors) = loaded.errors.clone();
        self.auto_enable_codemode.store(
            loaded.auto_enable_codemode.unwrap_or(true),
            Ordering::SeqCst,
        );
        self.warned_unreachable.store(false, Ordering::SeqCst);
        self.waited_for_startup.store(false, Ordering::SeqCst);
        *lock(&self.session_cwd) = cwd;
        self.session_active.store(true, Ordering::SeqCst);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *lock(&self.configured_entries) = loaded.servers.clone();
        let (registered, overridden) = self.registered_servers();
        *lock(&self.overridden) = overridden;
        // A resumed/reloaded loadout can name MCP tools that no server has
        // registered yet; keep them pending so a later registration attaches
        // (v1.0.0 `c662ec7e3`).
        {
            let registered: HashSet<String> =
                lock(&self.tools).definitions.keys().cloned().collect();
            let pending: HashSet<String> = self
                .api
                .get_active_tools()
                .unwrap_or_default()
                .into_iter()
                .filter(|name| name.starts_with("mcp__") && !registered.contains(name))
                .collect();
            *lock(&self.pending_tools) = pending;
        }
        {
            let mut servers: Vec<Arc<McpServer>> = loaded
                .servers
                .into_iter()
                .map(|entry| {
                    Arc::new(McpServer {
                        entry: Mutex::new(entry),
                        connection: Mutex::new(None),
                        registered_config: None,
                        message: Mutex::new(None),
                        ready: Mutex::new(None),
                    })
                })
                .collect();
            servers.extend(registered);
            *lock(&self.servers) = servers;
        }
        self.ensure_discovery_active(&ctx);
        let enabled: Vec<Arc<McpServer>> = lock(&self.servers)
            .iter()
            .filter(|server| lock(&server.entry).config.enabled())
            .cloned()
            .collect();
        if enabled.is_empty() {
            self.report_problems(&ctx, None);
            return;
        }
        for server in &enabled {
            let ready = self.start_connection(server.clone(), generation);
            *lock_server_ready(server) = Some(ready);
        }
    }

    /// `registeredServers` (index.ts:426).
    fn registered_servers(&self) -> (Vec<Arc<McpServer>>, Vec<String>) {
        let configured = lock(&self.configured_entries).clone();
        let mut registered = Vec::new();
        let mut overridden = Vec::new();
        for raw in self.api.get_mcp_servers().unwrap_or_default() {
            let name = raw
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let extension_path = raw
                .get("extensionPath")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_owned();
            let config = raw.get("config").cloned().unwrap_or(Value::Null);
            let validated = match validate_mcp_server_config(&name, &config) {
                Ok(config) => config,
                Err(error) => {
                    lock(&self.config_errors).push(format!(
                        "registered by {extension_path}: server \"{name}\": {error}"
                    ));
                    continue;
                }
            };
            if let Some(clash) = configured
                .iter()
                .find(|entry| mcp_namespace(&entry.name) == mcp_namespace(&name))
            {
                overridden.push(format!(
                    "\"{name}\" registered by {extension_path} is overridden by \"{}\" in {}",
                    clash.name, clash.source
                ));
                continue;
            }
            registered.push(Arc::new(McpServer {
                entry: Mutex::new(McpServerEntry {
                    name,
                    config: validated,
                    source: extension_path,
                    scope: Some(McpScope::Extension),
                }),
                connection: Mutex::new(None),
                registered_config: Some(config.to_string()),
                message: Mutex::new(None),
                ready: Mutex::new(None),
            }));
        }
        (registered, overridden)
    }

    /// `waitForDirectServers` (index.ts:987).
    async fn wait_for_direct_servers(self: &Arc<Self>, ctx: &ExtensionContext) {
        if self.waited_for_startup.swap(true, Ordering::SeqCst) {
            return;
        }
        let ready: Vec<watch::Receiver<bool>> = self
            .pending_servers()
            .into_iter()
            .filter(|(_, _, direct)| *direct)
            .map(|(_, ready, _)| ready)
            .collect();
        if ready.is_empty() {
            return;
        }
        let mut receivers = ready;
        let wait = async {
            for receiver in &mut receivers {
                let mut receiver = receiver.clone();
                if *receiver.borrow() {
                    continue;
                }
                let _ = receiver.wait_for(|ready| *ready).await;
            }
        };
        let finished = tokio::select! {
            () = wait => true,
            () = tokio::time::sleep(Duration::from_millis(DEFAULT_STARTUP_WAIT_MS)) => false,
        };
        if !finished && let Ok(ui) = ctx.ui() {
            ui.notify(
                "MCP servers are still connecting; their tools become available once connected.",
                NotifyType::Info,
            );
        }
    }

    /// `before_agent_start` handler (index.ts:1002).
    async fn on_before_agent_start(
        self: &Arc<Self>,
        event: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, String> {
        self.wait_for_direct_servers(ctx).await;
        let listings: Vec<McpServerListing> = lock(&self.servers)
            .iter()
            .map(|server| McpServerListing {
                entry: lock(&server.entry).clone(),
                instructions: server
                    .connection
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .and_then(|connection| connection.instructions()),
            })
            .collect();
        let section = render_servers_section(&listings);
        // Tools a resumed loadout named must have been re-registered by the
        // servers' connections by now; the next prompt clears the rest.
        lock(&self.pending_tools).clear();
        let _ = event;
        Ok(json!({
            "systemPromptOptions": {
                "sections": {
                    MCP_SERVERS_SECTION: section.map(Value::String).unwrap_or(Value::Null),
                },
            },
        }))
    }

    /// `tool_call` handler (index.ts:1016): wait for the servers a codemode
    /// script names, or for every server when a search/resource tool runs.
    async fn on_tool_call(
        self: &Arc<Self>,
        event: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, String> {
        let tool_name = event
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let pending = self.pending_servers();
        if pending.is_empty() {
            return Ok(Value::Null);
        }
        let is_codemode = tool_name == super::super::codemode::tool::CODEMODE_TOOL_NAME;
        let is_search = tool_name == super::super::tool_search::TOOL_SEARCH_TOOL_NAME;
        let waiting: Vec<watch::Receiver<bool>> = if is_codemode {
            let code = event
                .get("input")
                .and_then(|input| input.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            pending
                .into_iter()
                .filter(|(server, _, _)| {
                    let name = lock(&server.entry).name.clone();
                    script_needs_server(&code, &name)
                })
                .map(|(_, ready, _)| ready)
                .collect()
        } else if is_search || is_resource_tool(tool_name) {
            pending.into_iter().map(|(_, ready, _)| ready).collect()
        } else {
            Vec::new()
        };
        self.wait_for_servers(waiting, ctx.signal().unwrap_or(None))
            .await;
        Ok(Value::Null)
    }

    /// `mcp_servers_change` handler (index.ts:1055).
    async fn on_mcp_servers_change(
        self: &Arc<Self>,
        ctx: &ExtensionContext,
    ) -> Result<Value, String> {
        if !self.session_active.load(Ordering::SeqCst) {
            return Ok(Value::Null);
        }
        let generation = self.generation.load(Ordering::SeqCst);
        let (registered, overridden) = self.registered_servers();
        *lock(&self.overridden) = overridden;
        let next: HashMap<String, Option<String>> = registered
            .iter()
            .map(|server| {
                (
                    lock(&server.entry).name.clone(),
                    server.registered_config.clone(),
                )
            })
            .collect();
        let mut removed: Vec<Arc<McpServer>> = Vec::new();
        {
            let mut servers = lock(&self.servers);
            servers.retain(|server| {
                let entry = lock(&server.entry);
                if entry.scope != Some(McpScope::Extension) {
                    return true;
                }
                let keep = next
                    .get(&entry.name)
                    .is_some_and(|config| *config == server.registered_config);
                if !keep {
                    removed.push(server.clone());
                }
                keep
            });
            for server in &registered {
                let name = lock(&server.entry).name.clone();
                if !servers
                    .iter()
                    .any(|existing| lock(&existing.entry).name == name)
                {
                    servers.push(server.clone());
                }
            }
        }
        for server in &removed {
            let name = lock(&server.entry).name.clone();
            self.hide_tools(&name);
        }
        self.ensure_discovery_active(ctx);
        for server in &removed {
            let connection = server
                .connection
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(connection) = connection {
                connection.close().await;
            }
        }
        let added: Vec<Arc<McpServer>> = lock(&self.servers)
            .iter()
            .filter(|server| {
                let entry = lock(&server.entry);
                entry.scope == Some(McpScope::Extension)
                    && entry.config.enabled()
                    && server
                        .connection
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .is_none()
            })
            .cloned()
            .collect();
        if generation != self.generation.load(Ordering::SeqCst) || added.is_empty() {
            return Ok(Value::Null);
        }
        for server in &added {
            let ready = self.start_connection(server.clone(), generation);
            *lock_server_ready(server) = Some(ready);
        }
        for server in &added {
            let ready = lock_server_ready(server).clone();
            if let Some(mut ready) = ready {
                let _ = ready.wait_for(|ready| *ready).await;
            }
        }
        self.report_problems(ctx, Some(&added));
        Ok(Value::Null)
    }

    /// `session_shutdown` handler (index.ts:1101).
    async fn on_session_shutdown(self: &Arc<Self>) -> Result<Value, String> {
        self.session_active.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        let servers = lock(&self.servers).clone();
        *lock(&self.servers) = Vec::new();
        for server in servers {
            let connection = server
                .connection
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(connection) = connection {
                connection.close().await;
            }
        }
        Ok(Value::Null)
    }

    /// `/mcp` handler (index.ts:1122).
    async fn run_command(
        self: &Arc<Self>,
        args: String,
        ctx: ExtensionCommandContext,
    ) -> Result<(), String> {
        let base = ctx.base().clone();
        let parts: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
        let action = parts.first().cloned();
        let Some(action) = action else {
            return self.manage(&ctx).await;
        };
        if parts.len() > 2 {
            if let Ok(ui) = base.ui() {
                ui.notify(MCP_USAGE, NotifyType::Warning);
            }
            return Ok(());
        }
        let name = parts.get(1).cloned();
        match action.as_str() {
            "login" => {
                if let Some(server) = self.pick_for_oauth(&base, name.as_deref()) {
                    self.login_command(&base, &server).await;
                }
            }
            "logout" => {
                if let Some(server) = self.pick_for_oauth(&base, name.as_deref()) {
                    let name = lock(&server.entry).name.clone();
                    let removed = self.sign_out(&server).await;
                    if let Ok(ui) = base.ui() {
                        ui.notify(
                            &if removed {
                                format!("Signed out of MCP server \"{name}\".")
                            } else {
                                format!("No stored credentials for MCP server \"{name}\".")
                            },
                            NotifyType::Info,
                        );
                    }
                }
            }
            "reconnect" => {
                if let Some(server) = self.pick_for_reconnect(&base, name.as_deref()) {
                    let name = lock(&server.entry).name.clone();
                    let connection = server
                        .connection
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    match connection {
                        Some(connection) => match connection.reconnect().await {
                            Ok(()) => {
                                self.ensure_discovery_active(&base);
                                if let Ok(ui) = base.ui() {
                                    ui.notify(
                                        &format!(
                                            "Reconnected to MCP server \"{name}\" ({}).",
                                            describe_state(&server).unwrap_or_default()
                                        ),
                                        NotifyType::Info,
                                    );
                                }
                            }
                            Err(error) => {
                                if let Ok(ui) = base.ui() {
                                    ui.notify(&error.to_string(), NotifyType::Error);
                                }
                            }
                        },
                        None => {
                            if let Ok(ui) = base.ui() {
                                ui.notify(
                                    &format!("MCP server \"{name}\" is disabled."),
                                    NotifyType::Error,
                                );
                            }
                        }
                    }
                }
            }
            _ => {
                if let Ok(ui) = base.ui() {
                    ui.notify(MCP_USAGE, NotifyType::Warning);
                }
            }
        }
        Ok(())
    }

    fn pick_for_oauth(&self, ctx: &ExtensionContext, name: Option<&str>) -> Option<Arc<McpServer>> {
        if let Some(name) = name {
            let server = self.pick_server(name);
            if server.is_none() {
                if let Ok(ui) = ctx.ui() {
                    ui.notify(
                        &format!("No MCP server named \"{name}\"."),
                        NotifyType::Error,
                    );
                }
                return None;
            }
            let server = server.expect("checked");
            if !uses_oauth(&lock(&server.entry)) {
                if let Ok(ui) = ctx.ui() {
                    ui.notify(
                        "No enabled MCP server uses OAuth. Only HTTP servers without an Authorization header do.",
                        NotifyType::Error,
                    );
                }
                return None;
            }
            return Some(server);
        }
        let eligible: Vec<Arc<McpServer>> = self
            .server_snapshot()
            .into_iter()
            .filter(|server| uses_oauth(&lock(&server.entry)))
            .collect();
        futures::executor::block_on(pick_from_list(ctx, eligible, "MCP server"))
    }

    fn pick_for_reconnect(
        &self,
        ctx: &ExtensionContext,
        name: Option<&str>,
    ) -> Option<Arc<McpServer>> {
        if let Some(name) = name {
            let server = self.pick_server(name);
            if server.is_none()
                && let Ok(ui) = ctx.ui()
            {
                ui.notify(
                    &format!("No MCP server named \"{name}\"."),
                    NotifyType::Error,
                );
            }
            return server;
        }
        let eligible: Vec<Arc<McpServer>> = self
            .server_snapshot()
            .into_iter()
            .filter(|server| {
                server
                    .connection
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_some()
            })
            .collect();
        futures::executor::block_on(pick_from_list(ctx, eligible, "MCP server"))
    }

    /// `loginCommand` (index.ts:876).
    async fn login_command(self: &Arc<Self>, ctx: &ExtensionContext, server: &Arc<McpServer>) {
        let name = lock(&server.entry).name.clone();
        let Ok(ui) = ctx.ui() else {
            return;
        };
        if !ctx.has_ui().unwrap_or(false) {
            ui.notify(
                &format!("Signing in to MCP server \"{name}\" requires interactive mode."),
                NotifyType::Error,
            );
            return;
        }
        let tui_mode = ctx
            .mode()
            .map(|mode| mode == rpi_ext_host::types::ExtensionMode::Tui)
            .unwrap_or(false);
        let prompt = NotifySignInPrompt {
            ui: ui.clone(),
            name: name.clone(),
            tui_mode,
        };
        let failure = self.sign_in(server, &prompt).await;
        if let Some(failure) = failure {
            let kind = if failure == "Sign-in cancelled." {
                NotifyType::Info
            } else {
                NotifyType::Error
            };
            ui.notify(&failure, kind);
            return;
        }
        self.ensure_discovery_active(ctx);
        let tools = server
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|connection| connection.tools().len())
            .unwrap_or(0);
        ui.notify(
            &format!("Signed in to MCP server \"{name}\" ({tools} tools)."),
            NotifyType::Info,
        );
    }

    /// Manager loop (`/mcp` with no action, index.ts:791).
    async fn manage(self: &Arc<Self>, ctx: &ExtensionCommandContext) -> Result<(), String> {
        let base = ctx.base().clone();
        let Ok(ui) = base.ui() else {
            return Ok(());
        };
        if !ctx.has_ui().unwrap_or(false) {
            ui.notify(&self.format_status(), NotifyType::Info);
            return Ok(());
        }
        loop {
            let servers = self.server_snapshot();
            if servers.is_empty() {
                ui.notify(
                    &format!(
                        "No MCP servers configured. Add them to {} or .rpi/mcp.json.",
                        crate::config::get_agent_dir().join("mcp.json").display()
                    ),
                    NotifyType::Info,
                );
                return Ok(());
            }
            let Some(server) = pick_from_list(&base, servers, "MCP servers").await else {
                return Ok(());
            };
            let Some(action) = self.pick_action(&base, &server).await else {
                continue;
            };
            let name = lock(&server.entry).name.clone();
            match action.as_str() {
                "signin" => self.login_command(&base, &server).await,
                "tools" => {
                    let tools = server
                        .connection
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .as_ref()
                        .map(|connection| connection.tools())
                        .unwrap_or_default();
                    let description = tools
                        .iter()
                        .map(|tool| {
                            format!(
                                "{} [{}]",
                                tool.name,
                                super::config::get_mcp_tool_exposure(
                                    &lock(&server.entry).config,
                                    &tool.name
                                )
                                .as_str()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    ui.notify(
                        &format!("Tools of {name}:\n{description}"),
                        NotifyType::Info,
                    );
                }
                "exposure" => {
                    let exposures = ["codemode", "deferred", "direct", "hidden"];
                    let choice = ui
                        .select(
                            &format!("Exposure of {name}"),
                            &exposures.map(str::to_owned),
                            None,
                        )
                        .await;
                    if let Some(choice) = choice
                        && let Some(exposure) = McpExposure::parse(&choice)
                    {
                        let failure = self.set_exposure(&name, exposure);
                        *lock(&server.message) = failure.clone();
                        if let Some(failure) = failure {
                            ui.notify(&failure, NotifyType::Error);
                        }
                    }
                }
                "enable" | "disable" => {
                    let enable = action == "enable";
                    let failure = self.set_enabled(&base, &name, enable).await;
                    *lock(&server.message) = failure.clone();
                    if let Some(failure) = failure {
                        ui.notify(&failure, NotifyType::Error);
                    }
                }
                "reconnect" => {
                    let connection = server
                        .connection
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    if let Some(connection) = connection {
                        ui.notify(&format!("Reconnecting to {name}…"), NotifyType::Info);
                        if let Err(error) = connection.reconnect().await {
                            ui.notify(&error.to_string(), NotifyType::Error);
                        }
                    }
                }
                "signout" => {
                    self.sign_out(&server).await;
                }
                _ => {}
            }
        }
    }

    async fn pick_action(&self, ctx: &ExtensionContext, server: &Arc<McpServer>) -> Option<String> {
        let ui = ctx.ui().ok()?;
        let entry = lock(&server.entry).clone();
        let connection = server
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut options: Vec<String> = Vec::new();
        if !entry.config.enabled() {
            options.push("enable".to_owned());
        } else {
            let state = connection.as_ref().map(|connection| connection.state());
            if state == Some(ServerState::NeedsAuth) {
                options.push("signin".to_owned());
            }
            if state == Some(ServerState::Connected) {
                options.push("tools".to_owned());
            }
            if matches!(
                state,
                Some(
                    ServerState::Failed
                        | ServerState::Disconnected
                        | ServerState::Connected
                        | ServerState::NeedsAuth
                )
            ) {
                options.push("reconnect".to_owned());
            }
            if state == Some(ServerState::Connected)
                && connection
                    .as_ref()
                    .is_some_and(|connection| connection.oauth_url().is_some())
            {
                options.push("signout".to_owned());
            }
            options.push("exposure".to_owned());
            options.push("disable".to_owned());
        }
        if let Some(message) = lock(&server.message).clone() {
            ui.notify(&message, NotifyType::Warning);
        }
        let choice = ui
            .select(&format!("MCP server {}", entry.name), &options, None)
            .await?;
        Some(choice)
    }
}

async fn pick_from_list(
    ctx: &ExtensionContext,
    servers: Vec<Arc<McpServer>>,
    title: &str,
) -> Option<Arc<McpServer>> {
    let ui = ctx.ui().ok()?;
    if servers.len() == 1 {
        return servers.into_iter().next();
    }
    let labels: Vec<String> = servers
        .iter()
        .map(|server| {
            let entry = lock(&server.entry);
            let state = describe_state(server).unwrap_or_else(|| "disabled".to_owned());
            format!("{} ({state})", entry.name)
        })
        .collect();
    let choice = ui.select(title, &labels, None).await?;
    let name = choice
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    servers
        .into_iter()
        .find(|server| lock(&server.entry).name == name)
}

/// `McpSignInPrompt` over `ctx.ui` (index.ts:876 + ui.ts redirect view).
struct NotifySignInPrompt {
    ui: Arc<dyn rpi_ext_host::api::UiBridge>,
    name: String,
    tui_mode: bool,
}

#[async_trait::async_trait]
impl McpSignInPrompt for NotifySignInPrompt {
    fn show_authorization_url(&self, url: url::Url) {
        let text = if self.tui_mode {
            format!(
                "{}\n{}",
                rpi_tui::terminal_image::hyperlink(url.as_str(), url.as_str()),
                rpi_tui::terminal_image::hyperlink(
                    if cfg!(target_os = "macos") {
                        "Cmd+click to open"
                    } else {
                        "Ctrl+click to open"
                    },
                    url.as_str(),
                )
            )
        } else {
            url.as_str().to_owned()
        };
        self.ui.notify(
            &format!(
                "Sign in to MCP server \"{}\" in your browser:\n{text}",
                self.name
            ),
            NotifyType::Info,
        );
        open_browser(url.as_str());
    }

    async fn prompt_for_redirect_url(&self, signal: CancellationToken) -> Option<String> {
        let _ = signal;
        self.ui
            .input(
                &format!(
                    "Waiting for sign-in to \"{}\". If the browser cannot reach this machine, paste the URL it was redirected to.",
                    self.name
                ),
                Some("http://127.0.0.1:.../callback?code=..."),
                None,
            )
            .await
    }
}

/// Best-effort platform browser opener (upstream `openBrowser`).
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(target_os = "windows")]
    let command = "cmd";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let command = "xdg-open";
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new(command)
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new(command).arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::mcp::config::validate_mcp_server_config;

    fn entry(name: &str, config: Value) -> McpServerEntry {
        McpServerEntry {
            name: name.to_owned(),
            config: validate_mcp_server_config(name, &config).unwrap(),
            source: "test".to_owned(),
            scope: Some(McpScope::Global),
        }
    }

    #[test]
    fn renders_the_mcp_servers_section() {
        let servers = vec![
            McpServerListing {
                entry: entry(
                    "docs",
                    json!({"command": "x", "description": "Documentation search"}),
                ),
                instructions: None,
            },
            McpServerListing {
                entry: entry("direct-only", json!({"command": "x", "exposure": "direct"})),
                instructions: None,
            },
            McpServerListing {
                entry: entry("deferred", json!({"command": "x", "exposure": "deferred"})),
                instructions: None,
            },
        ];
        let section = render_servers_section(&servers).expect("section");
        assert!(
            section.starts_with("MCP servers whose tools are not declared to you."),
            "{section}"
        );
        assert!(
            section.contains("Call the tools of `codemode` servers"),
            "{section}"
        );
        assert!(
            section.contains("Load the tools of `tool_search` servers"),
            "{section}"
        );
        assert!(
            section.contains("- mcp__docs (codemode): Documentation search"),
            "{section}"
        );
        assert!(
            section.contains("- mcp__deferred (tool_search)"),
            "{section}"
        );
        assert!(!section.contains("direct-only"), "{section}");
    }

    #[test]
    fn section_prefers_instructions_and_omits_when_none() {
        let servers = vec![McpServerListing {
            entry: entry("s", json!({"command": "x"})),
            instructions: Some("First line\nsecond".to_owned()),
        }];
        let section = render_servers_section(&servers).unwrap();
        assert!(
            section.contains("mcp__s (codemode): First line"),
            "{section}"
        );
        assert!(render_servers_section(&[]).is_none());
        let direct = vec![McpServerListing {
            entry: entry("d", json!({"command": "x", "exposure": "direct"})),
            instructions: None,
        }];
        assert!(render_servers_section(&direct).is_none());
    }

    #[test]
    fn script_needs_server_detection() {
        assert!(script_needs_server(
            "await tools.mcp__docs__search({})",
            "docs"
        ));
        assert!(script_needs_server("return searchTools('x')", "docs"));
        assert!(script_needs_server("return ALL_TOOLS.length", "docs"));
        assert!(!script_needs_server("return 1", "docs"));
        assert!(!script_needs_server("await tools.other()", "docs"));
    }

    #[test]
    fn direct_and_indirect_exposures() {
        assert!(has_direct_tools(&entry(
            "a",
            json!({"command": "x", "exposure": "direct"})
        )));
        assert!(!has_direct_tools(&entry("b", json!({"command": "x"}))));
        assert!(has_indirect_tools(&entry("b", json!({"command": "x"}))));
        assert!(has_indirect_tools(&entry(
            "c",
            json!({"command": "x", "exposure": "hidden", "toolExposure": {"t": "deferred"}})
        )));
    }
}
