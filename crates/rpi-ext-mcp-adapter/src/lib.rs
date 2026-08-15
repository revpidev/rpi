//! rpi MCP adapter extension (L0 native plugin).
//!
//! Port of pi-mcp-adapter @ v2.24.0 (`3d953f9096bf8af05783a740c6608663a2c3180a`,
//! `rpi/external/pi-mcp-adapter`). This file mirrors `index.ts`: plugin
//! entry, `mcp` proxy tool + `mcp-config` flag registration, `tool_result`
//! error re-flagging, and the session lifecycle wiring.
//!
//! The crate is dual-target (design `docs/extensions/pi-mcp-adapter/
//! 02-design.md` §2.1): the `rlib` carries all logic for tests; the `cdylib`
//! is a thin `#[export_root_module]` shell over `rpi-ext-host`'s native ABI.
//!
//! Runtime discipline (TE02 task): never write to stdout (print-mode
//! contract); diagnostics go through `tracing` and must not contain
//! credentials (headers/bearer/OAuth tokens).

// abi_stable's `#[sabi(kind(Prefix(...)))]` generates a `<Name>_Ref` type.
#![allow(non_camel_case_types)]

pub mod approval;
pub mod cache;
pub mod commands;
pub mod config;
pub mod direct;
pub mod error;
pub mod guard;
pub mod lifecycle;
pub mod manager;
pub mod metadata;
pub mod oauth;
pub mod protocol;
pub mod proxy;
pub mod render;
pub mod runtime;
pub mod search;
pub mod session_recovery;
pub mod status;
pub mod tsshape;
pub mod utils;

use std::sync::{Arc, Mutex, OnceLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

use crate::direct::ToolSurface as _;
use crate::proxy::ProxyDispatcher;

/// Host-call handle + plugin-owned tokio runtime, established once by
/// `rpi_extension_init` and torn down on `session_shutdown` (design §2.3).
///
/// The cookie is stored as `usize` so `PluginState` stays `Send + Sync`
/// without an unsafe impl; it is an opaque host pointer that only ever
/// travels back into the host's trampoline unchanged.
struct PluginState {
    calls: RpiHostCalls,
    cookie: usize,
    runtime: runtime::PluginRuntime,
    dispatcher: Arc<ProxyDispatcher>,
    direct: Arc<Mutex<DirectSurface>>,
}

/// directTools surface state (index.ts install-scope variables):
/// registration registry, freeze flag, env override, early config.
struct DirectSurface {
    registry: direct::DirectToolRegistry,
    frozen: bool,
    env_override: Option<Vec<String>>,
    early_config: metadata::McpConfig,
    proxy_registered: bool,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// ToolSurface over host calls (design §3.8: unregisterTool first, active
/// tools fallback).
struct HostSurface<'a> {
    calls: &'a RpiHostCalls,
    cookie: usize,
}

impl crate::direct::ToolSurface for HostSurface<'_> {
    fn register_tool(&mut self, definition: Value) {
        host_call(self.calls, self.cookie, "registerTool", definition);
    }
    fn unregister_tool(&mut self, name: &str) -> bool {
        host_call_ok(
            self.calls,
            self.cookie,
            "unregisterTool",
            json!({"name": name}),
        )
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    }
    fn get_active_tools(&mut self) -> Option<Vec<String>> {
        // getActiveToolsIfReady (index.ts:172-180): errors during extension
        // loading map to None.
        host_call_ok(self.calls, self.cookie, "getActiveTools", json!({}))
            .and_then(|v| v.as_array().cloned())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
    }
    fn set_active_tools(&mut self, names: Vec<String>) {
        host_call(
            self.calls,
            self.cookie,
            "setActiveTools",
            json!({ "toolNames": names }),
        );
    }
}

/// `syncToolSurface` (index.ts:248-260): resolve direct tools from the
/// current config + cache, sync the surface, then apply the proxy-tool
/// truth table.
fn sync_tool_surface(state: &PluginState) {
    let runtime_config = state
        .dispatcher
        .try_runtime()
        .map(|runtime| runtime.config.clone());
    let (config, env_override, mut registry) = {
        let mut surface = state.direct.lock().unwrap_or_else(|e| e.into_inner());
        (
            runtime_config.unwrap_or_else(|| surface.early_config.clone()),
            surface.env_override.clone(),
            std::mem::take(&mut surface.registry),
        )
    };
    let cache = cache::load_metadata_cache(&cache::get_metadata_cache_path());
    let prefix = config.global_tool_prefix();
    let env_raw = std::env::var("MCP_DIRECT_TOOLS").ok();
    let env_selectors = if env_raw.as_deref() == Some("__none__") {
        Some(Vec::new())
    } else {
        env_override
    };
    let specs = if env_raw.as_deref() == Some("__none__") {
        Vec::new()
    } else {
        direct::resolve_direct_tools(&config, cache.as_ref(), prefix, env_selectors.as_deref())
    };
    let missing = cache::get_missing_configured_direct_tool_servers(
        &config,
        cache.as_ref(),
        if env_raw.is_none() {
            None
        } else {
            env_selectors.as_deref()
        },
        now_ms(),
    );

    let report = {
        let mut surface = HostSurface {
            calls: &state.calls,
            cookie: state.cookie,
        };
        registry.sync(&specs, &mut surface)
    };

    // syncProxyTool (index.ts:845-876).
    let should_register = direct::should_register_proxy_tool(&config, &specs, &missing);
    let mut proxy_registered = state
        .direct
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .proxy_registered;
    if should_register && !proxy_registered {
        let description = direct::build_proxy_description(&config, cache.as_ref(), &specs);
        let mut surface = HostSurface {
            calls: &state.calls,
            cookie: state.cookie,
        };
        surface.register_tool(json!({
            "definition": {
                "name": "mcp",
                "label": "MCP",
                "description": description,
                "promptSnippet": "MCP gateway — status, search, describe, auth, and single MCP tool calls",
                "parameters": proxy::tool_parameters_schema(),
                // renderMcpProxyToolCall (index.ts:698) + renderMcpToolResult
                // (index.ts:719): the host attaches the render closures and
                // dispatches {"kind":"render","what":"toolCall"|"toolResult"}
                // back here (host_call.rs:245-289).
                "renderCall": true,
                "renderResult": true,
            },
        }));
        proxy_registered = true;
    } else if !should_register && proxy_registered {
        let mut surface = HostSurface {
            calls: &state.calls,
            cookie: state.cookie,
        };
        if surface.unregister_tool("mcp") {
            proxy_registered = false;
        }
    }
    if report.added.len() + report.updated.len() + report.deactivated.len() > 0 {
        tracing::debug!(
            added = report.added.len(),
            updated = report.updated.len(),
            deactivated = report.deactivated.len(),
            "MCP: direct tools refreshed"
        );
    }

    let mut surface = state.direct.lock().unwrap_or_else(|e| e.into_inner());
    surface.registry = registry;
    surface.proxy_registered = proxy_registered;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `updateStatusBar` (init.ts:520-556) wired to `ui.setStatus`: computes
/// the "mcp" footer text from the ready runtime (no runtime yet → clear)
/// and publishes it to the host.
fn update_status_bar(state: &PluginState) {
    let text = state
        .dispatcher
        .try_runtime()
        .and_then(|runtime| status::build_status_bar_text(&runtime.config, &runtime.manager));
    set_status(state, text);
}

/// Publish (or clear, on `None`) the "mcp" footer status entry.
fn set_status(state: &PluginState, text: Option<String>) {
    host_call(
        &state.calls,
        state.cookie,
        "ui.setStatus",
        json!({ "key": "mcp", "text": text }),
    );
}

/// Shared host-call helper (same request envelope as `rpi-test-native-plugin`).
fn host_call(calls: &RpiHostCalls, cookie: usize, method: &str, args: Value) -> Value {
    let request = serde_json::to_vec(&json!({
        "call": method,
        "args": args,
        "seq": 0,
    }))
    .unwrap_or_default();
    let response = (calls.call)(cookie as PluginCookie, RVec::from(request));
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

fn pack(value: &Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

/// Host call returning the unwrapped `{"ok": value}` payload (`None` on
/// error or null).
fn host_call_ok(calls: &RpiHostCalls, cookie: usize, method: &str, args: Value) -> Option<Value> {
    let response = host_call(calls, cookie, method, args);
    if response.get("error").is_some() {
        return None;
    }
    let ok = response.get("ok").cloned().unwrap_or(Value::Null);
    if ok.is_null() {
        None
    } else {
        Some(ok)
    }
}

/// The session cwd via the host (`ctx.cwd`), falling back to the process
/// cwd (identical for the native in-process plugin in practice).
fn session_cwd(state: &PluginState) -> std::path::PathBuf {
    if let Some(cwd) = host_call_ok(&state.calls, state.cookie, "ctx.cwd", json!({})) {
        if let Some(cwd) = cwd.as_str() {
            if !cwd.is_empty() {
                return std::path::PathBuf::from(cwd);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let plugin_runtime = match runtime::PluginRuntime::start() {
        Ok(rt) => rt,
        Err(err) => {
            return json!({"error": {"kind": "init", "message": format!("tokio runtime: {err}")}});
        }
    };
    let dispatcher = Arc::new(ProxyDispatcher::new());
    let cwd_hint = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let early_config = config::load_mcp_config(None, &cwd_hint);
    let env_override = std::env::var("MCP_DIRECT_TOOLS").ok().and_then(|raw| {
        if raw == "__none__" {
            None
        } else {
            Some(
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<String>>(),
            )
        }
    });
    let state = PluginState {
        calls,
        cookie: cookie as usize,
        runtime: plugin_runtime,
        dispatcher,
        direct: Arc::new(Mutex::new(DirectSurface {
            registry: direct::DirectToolRegistry::default(),
            frozen: false,
            env_override,
            early_config,
            proxy_registered: false,
        })),
    };

    // Registrations (index.ts:283-441): flag, events, proxy tool.
    let cookie_usize = state.cookie;
    let calls = RpiHostCalls {
        call: state.calls.call,
    };
    let register = |method: &str, args: Value| -> Result<(), Value> {
        let response = host_call(&calls, cookie_usize, method, args);
        if response.get("error").is_some() {
            return Err(response);
        }
        Ok(())
    };

    if let Err(err) = register(
        "registerFlag",
        json!({
            "name": "mcp-config",
            "description": "Path to MCP config file",
            "type": "string",
        }),
    ) {
        return json!({"error": err});
    }
    for event in ["session_start", "session_shutdown", "tool_result"] {
        if let Err(err) = register("on", json!({"event": event})) {
            return json!({"error": err});
        }
    }

    // Config discovery runs from the session cwd (`ctx.cwd` host call;
    // process cwd as fallback). The tool surface (direct tools + proxy tool
    // truth table) syncs from the early config + metadata cache here
    // (index.ts:878-879), again after init completes, and on metadata
    // updates unless frozen.
    let cwd = session_cwd(&state);
    {
        let mut surface = state.direct.lock().unwrap_or_else(|e| e.into_inner());
        surface.early_config = config::load_mcp_config(None, &cwd);
    }
    if STATE.set(state).is_err() {
        return json!({"error": {"kind": "init", "message": "plugin already initialized"}});
    }
    let Some(plugin) = STATE.get() else {
        return json!({"error": {"kind": "init", "message": "plugin state missing"}});
    };
    // Initial tool-surface sync (direct tools from cache + proxy truth table).
    sync_tool_surface(plugin);
    let has_startup = plugin
        .direct
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .proxy_registered;
    let _ = has_startup;

    // Hooks (index.ts:302-329): post-init sync, then freeze if configured;
    // metadata updates respect the freeze; connect mode always syncs.
    plugin.dispatcher.set_hooks(proxy::DispatcherHooks {
        on_ready: Some(Arc::new(|| {
            if let Some(plugin) = STATE.get() {
                sync_tool_surface(plugin);
                update_status_bar(plugin);
                let freeze = plugin
                    .dispatcher
                    .try_runtime()
                    .and_then(|rt| rt.config.settings.clone())
                    .and_then(|s| s.get("freezeDirectTools").cloned())
                    .and_then(|v| v.as_bool())
                    == Some(true);
                if freeze {
                    plugin
                        .direct
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .frozen = true;
                }
                // state.onToolMetadataUpdated (index.ts:313-321): skipped
                // when frozen (prompt-cache red line R7). Every metadata
                // refresh is also a status-bar repaint point (upstream calls
                // updateStatusBar at the same sites).
                if let Some(runtime) = plugin.dispatcher.try_runtime() {
                    *runtime
                        .on_metadata_updated
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(|_server, _reason| {
                        if let Some(plugin) = STATE.get() {
                            let frozen = plugin
                                .direct
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .frozen;
                            if !frozen {
                                sync_tool_surface(plugin);
                            }
                            update_status_bar(plugin);
                        }
                    }));
                    // Transient lazy-connect status (init.ts:588-591).
                    *runtime
                        .on_connecting
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(|server| {
                        if let Some(plugin) = STATE.get() {
                            let text = plugin.dispatcher.try_runtime().and_then(|rt| {
                                status::format_status_bar_text(
                                    &rt.config,
                                    &format!("connecting to {server}..."),
                                )
                            });
                            set_status(plugin, text);
                        }
                    }));
                }
            }
        })),
        on_connect_sync: Some(Arc::new(|| {
            if let Some(plugin) = STATE.get() {
                sync_tool_surface(plugin);
                update_status_bar(plugin);
            }
        })),
    });

    // Load-time prewarm (index.ts:352-374 `startLoadTimeInitialization`):
    // only when an eager/keep-alive server exists.
    let prewarm = {
        let surface = plugin.direct.lock().unwrap_or_else(|e| e.into_inner());
        ProxyDispatcher::has_startup_server(&surface.early_config)
    };
    if prewarm {
        let dispatcher = plugin.dispatcher.clone();
        plugin.runtime.spawn(async move {
            dispatcher.start_init(cwd, None);
        });
    }
    json!({"ok": true})
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    pack(&install(calls, cookie))
}

pub extern "C" fn dispatch(_cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let message: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
    let Some(state) = STATE.get() else {
        return pack(&Value::Null);
    };
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute") => {
            let tool_name = message
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("");
            if tool_name != "mcp" {
                // Direct tool dispatch (direct-tools.ts createDirectToolExecutor).
                let spec = state
                    .direct
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .registry
                    .spec(tool_name)
                    .cloned();
                let Some(spec) = spec else {
                    return pack(&Value::Null);
                };
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let dispatcher = state.dispatcher.clone();
                let result = state.runtime.block_on(async move {
                    match dispatcher.current_direct().await {
                        Ok(runtime) => direct::execute_direct_tool(&runtime, &spec, &params).await,
                        Err(error_result) => error_result,
                    }
                });
                return pack(&result);
            }
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            // Native tool detection for the call not-found branch
            // (proxy-modes.ts:925-933, `getPiTools`).
            let native_tools = host_call_ok(&state.calls, state.cookie, "getAllTools", json!({}))
                .and_then(|ok| ok.as_array().cloned())
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let dispatcher = state.dispatcher.clone();
            let result = state
                .runtime
                .block_on(async move { dispatcher.execute(&params, &native_tools).await });
            pack(&result)
        }
        // Render protocol (host_call.rs:245-289): the host dispatches a
        // synchronous `{"kind":"render","what":"toolCall"|"toolResult",...}`
        // and expects a ComponentTree back. Pure JSON only — never touches
        // the plugin runtime. toolCall renders carry `toolName` (TE-D31):
        // the proxy "mcp" tool gets the proxy call lines, every direct tool
        // renders its own (prefixed) name as displayName.
        Some("render") => {
            match (
                message.get("what").and_then(Value::as_str),
                message.get("toolName").and_then(Value::as_str),
            ) {
                (Some("toolResult"), _) => {
                    let tree = render::render_mcp_tool_result(
                        message.get("result").unwrap_or(&Value::Null),
                        message.get("options").unwrap_or(&Value::Null),
                        message.get("context").unwrap_or(&Value::Null),
                    );
                    return pack(&tree);
                }
                (Some("toolCall"), Some("mcp")) => {
                    let tree = render::render_mcp_proxy_tool_call(
                        message
                            .get("context")
                            .and_then(|c| c.get("args"))
                            .unwrap_or(&Value::Null),
                    );
                    return pack(&tree);
                }
                (Some("toolCall"), Some(display_name)) => {
                    let tree = render::render_mcp_direct_tool_call(
                        display_name,
                        message
                            .get("context")
                            .and_then(|c| c.get("args"))
                            .unwrap_or(&Value::Null),
                    );
                    return pack(&tree);
                }
                _ => {}
            }
            pack(&Value::Null)
        }
        Some("event") => match message.get("event").and_then(Value::as_str) {
            Some("session_start") => {
                // index.ts:376-414: stop the previous runtime, then
                // re-initialize against the new session.
                state
                    .runtime
                    .block_on(state.dispatcher.clone().shutdown_owned());
                let cwd = session_cwd(state);
                let config_path = current_config_path(state);
                let dispatcher = state.dispatcher.clone();
                state.runtime.spawn(async move {
                    dispatcher.start_init(cwd, config_path);
                });
                pack(&Value::Null)
            }
            Some("session_shutdown") => {
                // G4 red line: all spawned MCP server processes are reaped
                // here (lifecycle graceful_shutdown -> manager close_all ->
                // stdio child shutdown). The "mcp" status bar entry is
                // cleared with the shutdown snapshot (publishMcpStatus-
                // Shutdown, mcp-status.ts:92-106 + updateStatusBar on the
                // empty runtime).
                state
                    .runtime
                    .block_on(state.dispatcher.clone().shutdown_owned());
                host_call(
                    &state.calls,
                    state.cookie,
                    "ui.setStatus",
                    json!({ "key": "mcp", "text": null }),
                );
                pack(&Value::Null)
            }
            Some("tool_result") => {
                // error-signal.ts: re-flag MCP tool failures so the host
                // records them as tool errors (TE01 hook:
                // ToolResultEventResult.isError).
                let details = message.get("payload").and_then(|p| p.get("details"));
                if let Some(details) = details {
                    if let Some(patch) = proxy::tool_error_override(details) {
                        return pack(&patch);
                    }
                    // TE-D04 compensation: upstream *throws* on invalid args
                    // (the host marks the tool call errored); the ABI has no
                    // throw channel, so the plugin returns an error result
                    // and re-flags it here.
                    if details.get("error").and_then(Value::as_str) == Some("invalid_args") {
                        return pack(&json!({ "isError": true }));
                    }
                }
                pack(&Value::Null)
            }
            _ => pack(&Value::Null),
        },
        _ => pack(&Value::Null),
    }
}

/// Read the `mcp-config` flag through the host (never logs the value: it is
/// a path, not a credential, but flag reads stay quiet anyway). The host
/// reply uses the `{"ok": value}` envelope, so unwrap via `host_call_ok`.
fn current_config_path(state: &PluginState) -> Option<String> {
    host_call_ok(
        &state.calls,
        state.cookie,
        "getFlag",
        json!({"name": "mcp-config"}),
    )
    .and_then(|v| v.as_str().map(str::to_string))
    .filter(|s| !s.is_empty())
}

/// The root module export (abi_stable).
#[abi_stable::export_root_module]
pub fn module() -> RpiNativeModule_Ref {
    RpiNativeModule {
        rpi_extension_init: init,
        rpi_dispatch: dispatch,
    }
    .leak_into_prefix()
}

/// Test hook: exercises `install` without the cdylib boundary.
#[doc(hidden)]
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

/// Test hook: the shared dispatcher (for integration tests driving the proxy
/// tool without the host).
#[doc(hidden)]
pub fn dispatcher_for_test() -> Option<Arc<ProxyDispatcher>> {
    STATE.get().map(|s| s.dispatcher.clone())
}
