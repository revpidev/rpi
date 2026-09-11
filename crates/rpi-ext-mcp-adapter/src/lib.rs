//! rpi MCP adapter extension (L0 native plugin).
//!
//! Port of pi-mcp-adapter @ v2.32.1 (`10a45367e033a32026987a75d6f401e37340c86f`,
//! v0.1.4 rebase; originally ported from v2.24.0/`3d953f90`, per-file provenance
//! headers record the source snapshot of each module), `rpi/external/pi-mcp-adapter`).
//! This file mirrors `index.ts`: plugin
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
pub mod bearer_store;
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
pub mod request_headers;
pub mod runtime;
pub mod search;
pub mod session_approvals;
pub mod session_recovery;
pub mod status;
pub mod truncate;
pub mod tsshape;
pub mod utils;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

use crate::direct::ToolSurface as _;
use crate::proxy::ProxyDispatcher;

/// Host-call channel to the CURRENT extension host. `RawLibrary::load_at`
/// dlopen-memoizes per path, so a second host in the same process re-runs
/// `install` on the SAME plugin statics — the channel (fn-pointer +
/// cookie) must follow the newest host while the tokio runtime and the
/// dispatcher stay process-lifetime (see `install`).
#[derive(Clone, Copy)]
struct HostChannel {
    call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
    cookie: usize,
}

impl HostChannel {
    /// Re-wrap the trampoline fn-pointer into the ABI handle.
    fn calls(&self) -> RpiHostCalls {
        RpiHostCalls { call: self.call }
    }
}

/// Host-call handle + plugin-owned tokio runtime, established once by
/// `rpi_extension_init`. The cookie is stored as `usize` so `PluginState`
/// stays `Send + Sync` without an unsafe impl; it is an opaque host pointer
/// that only ever travels back into the host's trampoline unchanged.
struct PluginState {
    host: RwLock<HostChannel>,
    runtime: runtime::PluginRuntime,
    dispatcher: Arc<ProxyDispatcher>,
    direct: Arc<Mutex<DirectSurface>>,
    /// `session_tree` leaf awaiting the next runtime init: outer `None` = no
    /// navigation recorded (restore from the file tip), `Some(None)` = the
    /// event reported a null leaf (empty branch), `Some(Some(id))` = target.
    pending_leaf: Mutex<Option<Option<String>>>,
}

impl PluginState {
    /// The current host channel (read at call time so background tasks and
    /// dispatcher hooks never act on a stale, dropped host).
    fn channel(&self) -> HostChannel {
        *self.host.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Adopt a freshly loaded host (session replacement: `/resume` `/new`
    /// `/fork` `/clone` `/import` build a fresh `NativeExtensionHost`).
    fn rebind(&self, channel: HostChannel) {
        *self.host.write().unwrap_or_else(|e| e.into_inner()) = channel;
    }
}

/// directTools surface state (index.ts install-scope variables):
/// registration registry, freeze flag, env override, early config.
struct DirectSurface {
    registry: direct::DirectToolRegistry,
    frozen: bool,
    env_override: Option<Vec<String>>,
    early_config: metadata::McpConfig,
    proxy_registered: bool,
    /// Last description passed to `registerTool` (index.ts:233/1189/1205
    /// `proxyToolDescription`): `syncProxyTool` re-registers when the pure
    /// description changes (R7.2.5.1/#432).
    proxy_description: Option<String>,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// Per-tool-call compact render state (R7.2.10.1/#349,
/// `McpToolRenderState` upstream rides the TUI render context): the ABI
/// context carries `toolCallId`, so the plugin owns the map — seeded by
/// `renderCall`, consumed (and dropped) by the final `renderResult`.
static COMPACT_RENDER_STATE: OnceLock<Mutex<HashMap<String, render::McpToolRenderState>>> =
    OnceLock::new();

fn compact_render_states() -> &'static Mutex<HashMap<String, render::McpToolRenderState>> {
    COMPACT_RENDER_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the render options from the CURRENT runtime config
/// (`resolveMcpToolRenderOptions(config.settings)`; fresh per dispatch so a
/// `/reload` with changed settings applies without reinstall).
fn current_render_options() -> render::McpToolRenderOptions {
    let settings = STATE.get().and_then(|state| {
        state
            .dispatcher
            .try_runtime()
            .map(|runtime| runtime.config.settings.clone())
    });
    render::resolve_mcp_tool_render_options(match settings.as_ref() {
        Some(settings) => settings.as_ref(),
        None => None,
    })
}

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
    let channel = state.channel();
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
    // `activeFailureServers` (index.ts:288-292 @ 10a45367): the resolver
    // drops a server's direct tools while it is inside the failure window
    // (R7.2.3.1/#434).
    let unavailable: std::collections::HashSet<String> = state
        .dispatcher
        .try_runtime()
        .map(|runtime| runtime.active_failure_servers())
        .unwrap_or_default();
    let specs = if env_raw.as_deref() == Some("__none__") {
        Vec::new()
    } else {
        direct::resolve_direct_tools(
            &config,
            cache.as_ref(),
            prefix,
            env_selectors.as_deref(),
            &unavailable,
        )
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
            calls: &channel.calls(),
            cookie: channel.cookie,
        };
        registry.sync(&specs, &mut surface)
    };

    // syncProxyTool (index.ts:1192-1219 @ 10a45367): register on first use and
    // re-register when the pure config description changes.
    let should_register = direct::should_register_proxy_tool(&config, &specs, &missing);
    let (mut proxy_registered, mut proxy_description) = {
        let surface = state.direct.lock().unwrap_or_else(|e| e.into_inner());
        (surface.proxy_registered, surface.proxy_description.clone())
    };
    if should_register {
        let description = direct::build_proxy_description(&config);
        if !proxy_registered || proxy_description.as_deref() != Some(description.as_str()) {
            let mut surface = HostSurface {
                calls: &channel.calls(),
                cookie: channel.cookie,
            };
            surface.register_tool(json!({
                "definition": {
                    "name": "mcp",
                    "label": "MCP",
                    "description": description,
                    "promptSnippet": "MCP gateway — status, search, describe, auth, and single MCP tool calls",
                    "parameters": proxy::tool_parameters_schema(),
                    // renderMcpProxyToolCall (tool-result-renderer.ts:304) + renderMcpToolResult
                    // (index.ts:231/284 @ 10a45367): the host attaches the render closures and
                    // dispatches {"kind":"render","what":"toolCall"|"toolResult"}
                    // back here (host_call.rs:245-289).
                    "renderCall": true,
                    "renderResult": true,
                },
            }));
            proxy_registered = true;
            proxy_description = Some(description);
        }
    } else if proxy_registered {
        let mut surface = HostSurface {
            calls: &channel.calls(),
            cookie: channel.cookie,
        };
        if surface.unregister_tool("mcp") {
            proxy_registered = false;
            proxy_description = None;
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
    surface.proxy_description = proxy_description;
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
    let channel = state.channel();
    host_call(
        &channel.calls(),
        channel.cookie,
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
    let channel = state.channel();
    if let Some(cwd) = host_call_ok(&channel.calls(), channel.cookie, "ctx.cwd", json!({})) {
        if let Some(cwd) = cwd.as_str() {
            if !cwd.is_empty() {
                return std::path::PathBuf::from(cwd);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let channel = HostChannel {
        call: calls.call,
        cookie: cookie as usize,
    };

    // Registrations (index.ts:283-441): flag + events — against THIS host,
    // for the first install and for a rebind alike (a fresh host starts
    // with an empty registry, so flags and event handlers must be
    // re-registered on it).
    let register = |method: &str, args: Value| -> Result<(), Value> {
        let response = host_call(&channel.calls(), channel.cookie, method, args);
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
    // Slash commands (R7.2.1.1): `/mcp` and `/mcp-auth` become reachable on
    // the ABI. Registered on every install/rebind, exactly like the flag and
    // the event handlers above (a fresh host starts with an empty registry).
    for (name, description) in commands::command_definitions() {
        if let Err(err) = register(
            "registerCommand",
            json!({"name": name, "description": description}),
        ) {
            return json!({"error": err});
        }
    }
    for event in [
        "input",
        "session_start",
        "session_shutdown",
        "session_tree",
        "tool_result",
    ] {
        if let Err(err) = register("on", json!({"event": event})) {
            return json!({"error": err});
        }
    }

    // Rebind path: session replacement (`/resume` `/new` `/fork` `/clone`
    // `/import`) builds a fresh `NativeExtensionHost` that re-loads this
    // same dlopen-memoized library and re-runs `install`. Upstream re-runs
    // the TS module per host; the native port keeps the process-lifetime
    // tokio runtime + dispatcher and RE-BINDS instead: adopt the new host
    // channel, reset the per-host tool surface (the fresh host registry is
    // empty — the old registry's "already registered" marks would suppress
    // re-registration), re-discover config from the (possibly new) session
    // cwd, and re-push the tool surface + status bar (the outgoing host's
    // `session_shutdown` already cleared the "mcp" footer entry; without
    // this rebind the load used to fail with "plugin already initialized"
    // and MCP tools, flags, events and the 🔌 status line all silently
    // vanished after /resume).
    if let Some(plugin) = STATE.get() {
        plugin.rebind(channel);
        {
            let mut surface = plugin.direct.lock().unwrap_or_else(|e| e.into_inner());
            let env_override = surface.env_override.clone();
            let early_config = surface.early_config.clone();
            *surface = DirectSurface {
                registry: direct::DirectToolRegistry::default(),
                frozen: false,
                env_override,
                early_config,
                proxy_registered: false,
                proxy_description: None,
            };
        }
        // Config discovery from the new session's cwd (ctx.cwd through the
        // NEW binding; argv override per #515 — a fresh host re-parses the
        // same process argv).
        let cwd = session_cwd(plugin);
        {
            let mut surface = plugin.direct.lock().unwrap_or_else(|e| e.into_inner());
            surface.early_config =
                config::load_mcp_config(crate::utils::get_config_path_from_argv().as_deref(), &cwd);
        }
        sync_tool_surface(plugin);
        update_status_bar(plugin);
        // Load-time prewarm mirrors the first install (start_init is
        // idempotent; a following session_start no-ops or re-arms it).
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
        spawn_bridge_retry();
        return json!({"ok": true});
    }

    let plugin_runtime = match runtime::PluginRuntime::start() {
        Ok(rt) => rt,
        Err(err) => {
            return json!({"error": {"kind": "init", "message": format!("tokio runtime: {err}")}});
        }
    };
    let dispatcher = Arc::new(ProxyDispatcher::new());
    let cwd_hint = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // #515 (utils.ts:67-88 @ 97253eb): read `--mcp-config[=]path` from the
    // HOST argv at load time — the host applies extension flag values only
    // after the final extension load (agent-session-services.ts:81-127), so
    // a getFlag read here would miss the override and the install-time
    // prewarm would connect servers from the wrong config layer stack.
    let early_config = config::load_mcp_config(
        crate::utils::get_config_path_from_argv().as_deref(),
        &cwd_hint,
    );
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
        host: RwLock::new(channel),
        runtime: plugin_runtime,
        dispatcher,
        direct: Arc::new(Mutex::new(DirectSurface {
            registry: direct::DirectToolRegistry::default(),
            frozen: false,
            env_override,
            early_config,
            proxy_registered: false,
            proxy_description: None,
        })),
        pending_leaf: Mutex::new(None),
    };

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
                    // R7.2.2.3: bind the session-approval sink/dialog and
                    // restore the active branch's grants (index.ts:214-227
                    // `restoreCurrentSessionApprovals` + init.ts:181-243;
                    // ABI first, JSONL fallback — `restore_session_approvals`).
                    runtime.approval.set_sink(Arc::new(HostSessionApprovalSink));
                    let approval_ui: Option<Arc<dyn approval::ApprovalHandler>> =
                        if ui_can_render_panel(plugin) {
                            Some(Arc::new(TuiApprovalHandler))
                        } else {
                            None
                        };
                    *runtime
                        .approval_ui
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = approval_ui;
                    let pending_leaf = plugin
                        .pending_leaf
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    match pending_leaf {
                        // No navigation recorded → session-load semantics
                        // (branch from the file tip).
                        None => restore_session_approvals(plugin, &runtime, None),
                        // Explicit null leaf → empty branch.
                        Some(None) => runtime.approval.restore(&[]),
                        Some(Some(leaf_id)) => {
                            restore_session_approvals(plugin, &runtime, Some(&leaf_id))
                        }
                    }
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
    // Bridge-ready retry push (2026-08-16 status-bar-loss fix): install()
    // (and every rebind) may run before the TUI binds its UI bridge, so an
    // early `update_status_bar` lands on the null bridge and is silently
    // dropped; with lazy servers `on_ready` only fires after the first real
    // use, so nothing re-pushes and the footer never shows the MCP entry.
    // Retry the push every 1.5 s until the host reports a UI (15 tries ≈
    // 22 s covers startup plus theme reloads); after that the regular
    // refresh hooks (on_ready / metadata updates / connect sync) keep it
    // current.
    spawn_bridge_retry();
    json!({"ok": true })
}

/// Arm the bridge-ready status-bar retry loop (see the install-site comment).
/// Idempotent and multi-armed safe: concurrent loops only re-push the same
/// idempotent footer text.
fn spawn_bridge_retry() {
    let Some(plugin) = STATE.get() else { return };
    plugin.runtime.spawn(async move {
        for _ in 0..15 {
            tokio::time::sleep(bridge_retry_interval()).await;
            let Some(state) = STATE.get() else { return };
            // V13-07 S2: only push the status bar once the host reports a
            // UI — before that every update_status_bar hits the null bridge
            // and is silently dropped, so the call is pure waste (up to 15
            // wasted pushes per install without it).
            let channel = state.channel();
            let has_ui = host_call_ok(&channel.calls(), channel.cookie, "ctx.hasUI", json!({}))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if !has_ui {
                continue;
            }
            update_status_bar(state);
            return;
        }
    });
}

/// V13-07 S2: the bridge-ready retry interval, 1500ms in production. Test
/// binaries override it (integration tests compile without cfg(test), so the
/// knob stays unconditional — production never touches it).
pub static BRIDGE_RETRY_MS_TEST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1500);

fn bridge_retry_interval() -> std::time::Duration {
    let ms = BRIDGE_RETRY_MS_TEST.load(std::sync::atomic::Ordering::Relaxed);
    std::time::Duration::from_millis(ms.max(1))
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
        // Slash-command dispatch (R7.2.1.1, [RPI-OWN]): the host registers
        // `/mcp` and `/mcp-auth` via `registerCommand` and forwards
        // `{"kind":"command","name","args"}`; `command.*` host calls are
        // legal inside this dispatch only. Unknown names return an error
        // result — never a panic and never a hang.
        Some("command") => {
            let name = message.get("name").and_then(Value::as_str).unwrap_or("");
            let args = message.get("args").and_then(Value::as_str).unwrap_or("");
            pack(&handle_command(state, name, args))
        }
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
            // V13-07 S3: native tool detection for the call not-found branch
            // (proxy-modes.ts:925-933, `getPiTools`) is LAZY — the
            // getAllTools host call runs only when a tool name fails to
            // resolve to a server tool; the common mcp dispatch pays zero
            // host calls for it.
            let channel = state.channel(); // fn-pointer + cookie — Copy
            let native_tools: crate::proxy::NativeToolsResolver = Arc::new(move || {
                // The host trampoline is a plain fn-pointer: re-wrap it in a
                // short-lived RpiHostCalls to reuse host_call_ok.
                let calls = RpiHostCalls { call: channel.call };
                host_call_ok(&calls, channel.cookie, "getAllTools", json!({}))
                    .and_then(|ok| ok.as_array().cloned())
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(|t| {
                                t.get("name").and_then(Value::as_str).map(str::to_string)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            let dispatcher = state.dispatcher.clone();
            let result = state.runtime.block_on(async move {
                dispatcher
                    .execute_with_resolver(&params, native_tools)
                    .await
            });
            pack(&result)
        }
        // Render protocol (host_call.rs:245-289): the host dispatches a
        // synchronous `{"kind":"render","what":"toolCall"|"toolResult",...}`
        // and expects a ComponentTree back. Pure JSON only — never touches
        // the plugin runtime. toolCall renders carry `toolName` (TE-D31):
        // the proxy "mcp" tool gets the proxy call lines, every direct tool
        // renders its own (prefixed) name as displayName.
        Some("render") => {
            let render_options = current_render_options();
            let tool_call_id = message
                .get("context")
                .and_then(|c| c.get("toolCallId"))
                .and_then(Value::as_str)
                .map(str::to_string);
            match (
                message.get("what").and_then(Value::as_str),
                message.get("toolName").and_then(Value::as_str),
            ) {
                (Some("toolResult"), _) => {
                    // Consume the compact state on the FINAL render (partials
                    // keep it for the upcoming final row).
                    let is_partial = message
                        .get("options")
                        .and_then(|o| o.get("isPartial"))
                        .and_then(Value::as_bool)
                        == Some(true);
                    let compact_state = tool_call_id.and_then(|id| {
                        let mut states = compact_render_states()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if is_partial {
                            states.get(&id).cloned()
                        } else {
                            states.remove(&id)
                        }
                    });
                    let tree = render::render_mcp_tool_result(
                        message.get("result").unwrap_or(&Value::Null),
                        message.get("options").unwrap_or(&Value::Null),
                        message.get("context").unwrap_or(&Value::Null),
                        &render_options,
                        compact_state.as_ref(),
                    );
                    return pack(&tree);
                }
                (Some("toolCall"), Some("mcp")) => {
                    let args = message
                        .get("context")
                        .and_then(|c| c.get("args"))
                        .unwrap_or(&Value::Null);
                    let context = message.get("context").unwrap_or(&Value::Null);
                    if let Some(id) = &tool_call_id {
                        let mut states = compact_render_states()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let entry = states.entry(id.clone()).or_default();
                        let lines = render::format_mcp_proxy_tool_call_lines(
                            args,
                            render::DEFAULT_MAX_CALL_INPUT_CHARS,
                        );
                        let tree = render::render_tool_call_with_state(
                            &lines,
                            &render_options,
                            Some(context),
                            Some(entry),
                        );
                        return pack(&tree);
                    }
                    let tree = render::render_mcp_proxy_tool_call(args);
                    return pack(&tree);
                }
                (Some("toolCall"), Some(display_name)) => {
                    let args = message
                        .get("context")
                        .and_then(|c| c.get("args"))
                        .unwrap_or(&Value::Null);
                    let context = message.get("context").unwrap_or(&Value::Null);
                    if let Some(id) = &tool_call_id {
                        let mut states = compact_render_states()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let entry = states.entry(id.clone()).or_default();
                        let lines = render::format_mcp_direct_tool_call_lines(
                            display_name,
                            args,
                            render::DEFAULT_MAX_CALL_INPUT_CHARS,
                        );
                        let tree = render::render_tool_call_with_state(
                            &lines,
                            &render_options,
                            Some(context),
                            Some(entry),
                        );
                        return pack(&tree);
                    }
                    let tree = render::render_mcp_direct_tool_call(display_name, args);
                    return pack(&tree);
                }
                _ => {}
            }
            pack(&Value::Null)
        }
        Some("event") => match message.get("event").and_then(Value::as_str) {
            Some("input") => {
                // `pi.on("input")` (index.ts:683-706 @ 10a45367, R7.2.7.1):
                // converge the keep-alive fleet before the input is
                // processed. `current()` doubles as the bounded init wait
                // (upstream `awaitWithTimeout(initPromise,
                // INIT_WAIT_TIMEOUT_MS=30s)`); an init failure returns
                // quietly (upstream `catch { return }`).
                let dispatcher = state.dispatcher.clone();
                state.runtime.block_on(async move {
                    if let Ok(runtime) = dispatcher.current().await {
                        runtime.lifecycle.ensure_converged().await;
                    }
                });
                pack(&Value::Null)
            }
            Some("session_start") => {
                // index.ts:376-414: stop the previous runtime, then
                // re-initialize against the new session. A fresh session has
                // no recorded tree navigation; restore from the file tip
                // (`buildSessionPath` default) unless a `session_tree` event
                // arrives before init completes.
                *state.pending_leaf.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
                let channel = state.channel();
                host_call(
                    &channel.calls(),
                    channel.cookie,
                    "ui.setStatus",
                    json!({ "key": "mcp", "text": null }),
                );
                pack(&Value::Null)
            }
            Some("session_tree") => {
                // R7.2.2.3/.5: branch navigation → rebuild the approval set
                // for the target branch (index.ts:728-745 @ 928c30c). The
                // leaf id rides the `session_tree` payload (TE21 host parity
                // fix; consumed by the JSONL fallback path — the ABI path
                // reads the host's own active branch) and `newLeafId: null`
                // means an empty branch. When the runtime is still
                // initializing, the leaf is kept for the on_ready restore.
                let payload = message.get("payload");
                let explicit_null = payload
                    .and_then(|payload| payload.get("newLeafId"))
                    .is_some_and(Value::is_null);
                let new_leaf = payload
                    .and_then(|payload| payload.get("newLeafId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                *state.pending_leaf.lock().unwrap_or_else(|e| e.into_inner()) =
                    match (new_leaf.clone(), explicit_null) {
                        (Some(leaf_id), _) => Some(Some(leaf_id)),
                        (None, true) => Some(None),
                        (None, false) => None,
                    };
                if let Some(runtime) = state.dispatcher.try_runtime() {
                    if explicit_null {
                        runtime.approval.restore(&[]);
                    } else {
                        restore_session_approvals(state, &runtime, new_leaf.as_deref());
                    }
                }
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
    let channel = state.channel();
    host_call_ok(
        &channel.calls(),
        channel.cookie,
        "getFlag",
        json!({"name": "mcp-config"}),
    )
    .and_then(|v| v.as_str().map(str::to_string))
    .filter(|s| !s.is_empty())
}

// ============================================================================
// Session approval persistence + TUI dialog (R7.2.2.3/.4, TE21)
// ============================================================================

/// `canRenderPanel`-equivalent gate for the approval dialog: a blocking
/// `ui.select` only settles on a real TUI bridge (same rule as the `/mcp`
/// panel, upstream #365 / `commands.ts`:38-47 @ `10a45367`).
fn ui_can_render_panel(state: &PluginState) -> bool {
    let channel = state.channel();
    let calls = RpiHostCalls { call: channel.call };
    let host = CommandHost {
        calls: &calls,
        cookie: channel.cookie,
    };
    host.can_render_panel()
}

/// Active-branch approval restore (`restoreCurrentSessionApprovals`,
/// index.ts:217-229 @ 928c30c): **ABI first, JSONL fallback**
/// (ADR-0027 / TE33, R7.2.2.5).
///
/// - ABI path: `ctx.sessionEntries {customType:"mcp-approval-v1"}` — the
///   authoritative in-memory active branch, covering file **and in-memory**
///   (`--no-session`) sessions alike (TE-D41 closed on this path) and
///   winning over any file tail (multi-process hosts are read from memory,
///   not the JSONL tip). No `limit` is sent: restore wants every grant on
///   the branch (task §8.3-1; recorded in §7).
/// - Old hosts answer `unknownMethod` → the TE21 transitional JSONL read
///   (`ctx.sessionFile.path` + `session_tree.newLeafId` branch walk,
///   04-design §6.2); in-memory sessions stay fail-closed clear there
///   (documented TE-D41 fallback limitation).
/// - Any other ABI error — or a reply that is not an entry array — clears
///   the set with a debug log: the same posture as upstream's `getBranch()`
///   catch (index.ts:225-228) and the TE21 IO-error path; never a panic and
///   never silently wider than the branch.
///
/// Probing is per call — nothing is cached, so every restore re-detects ABI
/// availability and a host reload/switch re-probes naturally (task §4.2 A8).
fn restore_session_approvals(
    state: &PluginState,
    runtime: &proxy::McpRuntime,
    leaf_id: Option<&str>,
) {
    match read_active_branch_approvals(&state.channel(), leaf_id) {
        BranchApprovalRead::Entries(entries) => runtime.approval.restore(&entries),
        BranchApprovalRead::Unavailable => runtime.approval.restore(&[]),
    }
}

/// The outcome of one active-branch approval read.
enum BranchApprovalRead {
    /// A source served the branch (ABI or JSONL): replay these grants.
    Entries(Vec<session_approvals::SessionApprovalEntry>),
    /// No source could serve it (ABI error other than `unknownMethod`,
    /// malformed reply, absent/unreadable JSONL): the restore clears
    /// (fail-closed).
    Unavailable,
}

fn read_active_branch_approvals(
    channel: &HostChannel,
    leaf_id: Option<&str>,
) -> BranchApprovalRead {
    // ABI first (ADR-0027): the host replays the active branch from memory.
    let reply = host_call(
        &channel.calls(),
        channel.cookie,
        "ctx.sessionEntries",
        json!({"customType": session_approvals::MCP_APPROVAL_CUSTOM_TYPE}),
    );
    if let Some(error) = reply.get("error") {
        if error.get("kind").and_then(Value::as_str) != Some("unknownMethod") {
            tracing::debug!("MCP: could not read session approvals over the ABI: {error}");
            return BranchApprovalRead::Unavailable;
        }
        // `unknownMethod` → old host; fall through to the JSONL read below.
    } else {
        return match reply.get("ok").and_then(Value::as_array) {
            Some(entries) => BranchApprovalRead::Entries(
                session_approvals::approval_entries_from_session_entries(entries),
            ),
            None => {
                // A conformant host always answers an entry array (V14-25);
                // a null/absent `ok` is a protocol anomaly → fail-closed.
                tracing::debug!("MCP: malformed ctx.sessionEntries reply");
                BranchApprovalRead::Unavailable
            }
        };
    }

    // TE21 transitional path (old hosts, 04-design §6.2): straight JSONL
    // read of the authoritative `ctx.sessionFile` path. In-memory sessions
    // have no file to replay and clear here (documented TE-D41 fallback
    // limitation; upstream rebuilds from memory instead).
    let info = host_call_ok(
        &channel.calls(),
        channel.cookie,
        "ctx.sessionFile",
        json!({}),
    );
    let path = info
        .as_ref()
        .and_then(|info| info.get("path"))
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let Some(path) = path else {
        return BranchApprovalRead::Unavailable;
    };
    match session_approvals::read_session_approval_entries(std::path::Path::new(path), leaf_id) {
        Ok(entries) => BranchApprovalRead::Entries(entries),
        Err(error) => {
            tracing::debug!("MCP: could not read session approvals: {error}");
            BranchApprovalRead::Unavailable
        }
    }
}

/// Host `appendEntry` sink (session-approvals.ts:89 `createSessionApprovalWriter`
/// @ 928c30c): fail-soft, never propagates into the approval decision.
struct HostSessionApprovalSink;

impl session_approvals::SessionApprovalSink for HostSessionApprovalSink {
    fn append(&self, entry: &session_approvals::SessionApprovalEntry) {
        let Some(state) = STATE.get() else {
            return;
        };
        let channel = state.channel();
        let reply = host_call(
            &channel.calls(),
            channel.cookie,
            "appendEntry",
            json!({
                "customType": session_approvals::MCP_APPROVAL_CUSTOM_TYPE,
                "data": session_approvals::entry_to_value(entry),
            }),
        );
        if let Some(error) = reply.get("error") {
            tracing::debug!("MCP: failed to persist session approval: {error}");
        }
    }
}

/// Built-in three-choice dialog (tool-approval.ts:150-165 @ 928c30c):
/// sanitized server/tool title + bounded pretty-printed argument preview.
struct TuiApprovalHandler;

impl approval::ApprovalHandler for TuiApprovalHandler {
    fn decide(
        &self,
        server_name: &str,
        tool: &metadata::ToolMetadata,
        args: &Value,
        _origin: approval::ApprovalOrigin,
    ) -> approval::ApprovalDecision {
        let Some(state) = STATE.get() else {
            return approval::ApprovalDecision::Deny;
        };
        let channel = state.channel();
        let preview = approval::dialog_preview(args);
        let title = format!(
            "MCP: {} wants to run {}",
            utils::sanitize_terminal_text(server_name),
            utils::sanitize_terminal_text(&tool.original_name)
        );
        let options = vec![
            "Allow once".to_string(),
            "Allow for session".to_string(),
            "Deny".to_string(),
        ];
        let selected = host_call_ok(
            &channel.calls(),
            channel.cookie,
            "ui.select",
            json!({"title": format!("{title}\n\nArguments:\n{preview}"), "options": options}),
        );
        match selected
            .and_then(|value| value.as_str().map(str::to_string))
            .as_deref()
        {
            Some("Allow once") => approval::ApprovalDecision::AllowOnce,
            Some("Allow for session") => approval::ApprovalDecision::AllowForSession,
            _ => approval::ApprovalDecision::Deny,
        }
    }
}

// `sanitizeTerminalText` / `stripOscSequences` live in `utils` (upstream
// `utils.ts:206-238 @ 10a45367`).

// ============================================================================
// Slash commands (`/mcp`, `/mcp-auth`) — R7.2.1.1/.3/.4, [RPI-OWN]
// ============================================================================

/// Host-call facade for command handlers. Mirrors the upstream command
/// context surface (`ctx.hasUI` / `ctx.mode` / `ctx.ui.notify|select|
/// setStatus`) using only existing ABI methods (`ui.*`, capability `ui`).
struct CommandHost<'a> {
    calls: &'a RpiHostCalls,
    cookie: usize,
}

impl CommandHost<'_> {
    fn has_ui(&self) -> bool {
        host_call_ok(self.calls, self.cookie, "ctx.hasUI", json!({}))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn mode(&self) -> String {
        host_call_ok(self.calls, self.cookie, "ctx.mode", json!({}))
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// `canRenderPanel` (commands.ts @ v2.32.1 `10a45367`:38-47, upstream
    /// #365 — absent from the v2.24.0 port baseline): `hasUI` alone is not
    /// enough — rpc/print/json bind a headless bridge whose `ui.select` never
    /// settles, so an overlay/select additionally requires
    /// `ctx.mode === "tui"`.
    fn can_render_panel(&self) -> bool {
        self.has_ui() && self.mode() == "tui"
    }

    fn notify(&self, message: &str, kind: &str) {
        if !self.has_ui() {
            return;
        }
        host_call(
            self.calls,
            self.cookie,
            "ui.notify",
            json!({"message": message, "notifyType": kind}),
        );
    }

    /// Show a select dialog (status view). The selection is informational and
    /// deliberately discarded; the host blocks until the dialog resolves.
    fn select(&self, title: &str, options: &[String]) {
        host_call_ok(
            self.calls,
            self.cookie,
            "ui.select",
            json!({"title": title, "options": options}),
        );
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        if !self.has_ui() {
            return;
        }
        host_call(
            self.calls,
            self.cookie,
            "ui.setStatus",
            json!({"key": key, "text": text}),
        );
    }
}

/// Resolve the ready runtime, awaiting an in-flight init (upstream
/// `await initPromise`). `NotStarted`/`Failed` map to the same error results
/// the proxy/direct tools return.
fn command_runtime(state: &PluginState) -> Result<Arc<proxy::McpRuntime>, Value> {
    state.runtime.block_on(state.dispatcher.current_direct())
}

/// `{"kind":"command"}` dispatch entry (see the dispatch arm). Unknown
/// names return an error result — never a panic and never a hang (R7.2.1.4).
fn handle_command(state: &PluginState, name: &str, args: &str) -> Value {
    match name {
        "mcp" => handle_mcp_command(state, args),
        "mcp-auth" => handle_mcp_auth_command(state, args),
        other => commands::error_result(format!("Unknown command: {other}"), "unknown_command"),
    }
}

/// `/mcp [status|tools|enable|disable|reconnect|logout]` (FR-A/FR-B/FR-D).
fn handle_mcp_command(state: &PluginState, args: &str) -> Value {
    let channel = state.channel();
    let calls = RpiHostCalls { call: channel.call };
    let host = CommandHost {
        calls: &calls,
        cookie: channel.cookie,
    };
    let (subcommand, target) = commands::parse_subcommand(args);

    match subcommand {
        commands::McpSubcommand::Status => {
            let runtime = match command_runtime(state) {
                Ok(runtime) => runtime,
                Err(result) => return result,
            };
            let (config, metadata, _) = proxy::search_state_snapshot(&runtime);
            let text = commands::format_status_text(
                &config,
                &runtime.manager,
                &metadata,
                &runtime.failures,
            );
            if host.can_render_panel() {
                let mut options: Vec<String> = text
                    .lines()
                    .map(str::to_string)
                    .filter(|line| !line.trim().is_empty())
                    .collect();
                options.push("Close".to_string());
                host.select("MCP Server Status", &options);
            } else {
                // RPC keeps a real bridge (notify reaches the client);
                // print/json bind the null bridge, where the returned text is
                // the observable command output (print-mode stdout contract:
                // the plugin never writes stdout, lib.rs:9-11).
                host.notify(&text, "info");
            }
            commands::text_result(text)
        }
        commands::McpSubcommand::Tools => {
            let runtime = match command_runtime(state) {
                Ok(runtime) => runtime,
                Err(result) => return result,
            };
            let (config, metadata, unavailable) = proxy::search_state_snapshot(&runtime);
            let unavailable: Vec<String> = unavailable.into_iter().collect();
            let text = commands::format_tools_text(&config, &metadata, &unavailable);
            host.notify(&text, "info");
            commands::text_result(text)
        }
        commands::McpSubcommand::Enable | commands::McpSubcommand::Disable => {
            let disabled = matches!(subcommand, commands::McpSubcommand::Disable);
            let action = if disabled { "disable" } else { "enable" };
            let Some(server) = target else {
                let text = format!("Usage: /mcp {action} <server>");
                host.notify(&text, "error");
                return commands::error_result(text, "invalid_args");
            };
            let runtime = match command_runtime(state) {
                Ok(runtime) => runtime,
                Err(result) => return result,
            };
            if !runtime.config.mcp_servers.contains_key(&server) {
                let text = format!("Server \"{server}\" not found in effective config");
                host.notify(&text, "error");
                return commands::error_result(text, "server_not_found");
            }
            // FR-D: project-level `<cwd>/.rpi/mcp.json`, read-modify-write,
            // unknown fields preserved, atomic tmp+rename (ADR-0001 path).
            let cwd = session_cwd(state);
            match commands::write_project_server_disabled_override(&cwd, &server, disabled) {
                Ok((path, changed)) => {
                    let text = commands::enable_disable_message(&server, disabled, changed, &path);
                    host.notify(&text, "info");
                    commands::text_result_with_details(
                        text,
                        json!({
                            "server": server,
                            "disabled": disabled,
                            "changed": changed,
                            "path": path.to_string_lossy(),
                        }),
                    )
                }
                Err(error) => {
                    let text = format!(
                        "Failed to update the project MCP override for \"{server}\": {error}"
                    );
                    host.notify(&text, "error");
                    commands::error_result(text, "write_failed")
                }
            }
        }
        commands::McpSubcommand::Reconnect => {
            let runtime = match command_runtime(state) {
                Ok(runtime) => runtime,
                Err(result) => return result,
            };
            let names: Vec<String> = match target {
                Some(server) => {
                    if !runtime.config.mcp_servers.contains_key(&server) {
                        let text = format!("Server \"{server}\" not found in config");
                        host.notify(&text, "error");
                        return commands::error_result(text, "server_not_found");
                    }
                    vec![server]
                }
                None => runtime.config.mcp_servers.keys().cloned().collect(),
            };
            let mut lines = Vec::new();
            for name in names {
                lines.push(reconnect_server(state, &host, &runtime, &name));
            }
            update_status_bar(state);
            commands::text_result(lines.join("\n"))
        }
        commands::McpSubcommand::Logout => {
            let Some(server) = target else {
                let text = "Usage: /mcp logout <server>".to_string();
                host.notify(&text, "error");
                return commands::error_result(text, "invalid_args");
            };
            let runtime = match command_runtime(state) {
                Ok(runtime) => runtime,
                Err(result) => return result,
            };
            if !runtime.config.mcp_servers.contains_key(&server) {
                let text = format!("Server \"{server}\" not found in config");
                host.notify(&text, "error");
                return commands::error_result(text, "server_not_found");
            }
            let options = crate::oauth::store::AuthStorageOptions {
                base_dir: oauth_dir(&runtime),
            };
            match crate::oauth::remove_auth(&server, &options) {
                Ok(()) => {
                    state.runtime.block_on(runtime.manager.close(&server));
                    update_status_bar(state);
                    let text = format!(
                        "OAuth credentials cleared for \"{server}\". Run /mcp-auth {server} to authenticate again."
                    );
                    host.notify(&text, "info");
                    commands::text_result(text)
                }
                Err(error) => {
                    let text =
                        format!("Failed to clear OAuth credentials for \"{server}\": {error}");
                    host.notify(&text, "error");
                    commands::error_result(text, "logout_failed")
                }
            }
        }
        commands::McpSubcommand::Unknown(other) => {
            let text = format!("Unknown /mcp subcommand: {other}\n{}", commands::MCP_USAGE);
            host.notify(&text, "error");
            commands::error_result(text, "unknown_subcommand")
        }
    }
}

/// `reconnectServer` (commands.ts:138-210 @ `3d953f90`, v2.24.0): close +
/// connect, refresh metadata/cache/status, clear the failure record.
/// Returns the text line.
fn reconnect_server(
    state: &PluginState,
    host: &CommandHost<'_>,
    runtime: &Arc<proxy::McpRuntime>,
    name: &str,
) -> String {
    let Some(definition) = runtime.config.mcp_servers.get(name).cloned() else {
        let text = format!("Server \"{name}\" not found in config");
        host.notify(&text, "error");
        return text;
    };
    if definition.is_disabled() {
        let text = format!("MCP: {name} is disabled. Run /mcp enable {name}, then /reload.");
        host.notify(&text, "warning");
        return text;
    }
    let outcome = state.runtime.block_on(async {
        runtime.manager.close(name).await;
        runtime.manager.connect(name, &definition).await
    });
    match outcome {
        Ok(connection) => match connection.status() {
            crate::manager::ConnectionStatus::Connected => {
                proxy::update_server_metadata(runtime, name);
                proxy::update_metadata_cache(runtime, name);
                // commands.ts:224 @ 10a45367: a reconnect clears the
                // failure window with a reason; the plain notify covers the
                // no-active-window case.
                if !runtime
                    .failures
                    .clear_with_reason(name, "command-reconnect")
                {
                    proxy::notify_metadata_updated(runtime, name, "command-reconnect");
                }
                proxy::mark_keep_alive_after_connect(runtime, name);
                let text = format!(
                    "MCP: Reconnected to {name} ({} tools, {} resources)",
                    connection.tools_len(),
                    connection.resources_snapshot().len()
                );
                host.notify(&text, "info");
                text
            }
            crate::manager::ConnectionStatus::NeedsAuth => {
                let text = format!("MCP: {name} requires OAuth. Run /mcp-auth {name} first.");
                host.notify(&text, "warning");
                text
            }
            _ => {
                let text =
                    format!("MCP: Failed to reconnect to {name}: connection did not become ready");
                host.notify(&text, "error");
                text
            }
        },
        Err(error) => {
            let message = error.to_string();
            runtime
                .failures
                .record(name, &message, runtime.owner_cancel.clone());
            let text = format!("MCP: Failed to reconnect to {name}: {message}");
            host.notify(&text, "error");
            text
        }
    }
}

/// `/mcp-auth <server>` (R7.2.1.1/.4): interactive OAuth only. A headless
/// session returns the guidance text immediately — the #365 no-hang contract
/// (`authenticateServer` commands.ts:232-243 @ `3d953f90`, no-UI message at
/// :242).
fn handle_mcp_auth_command(state: &PluginState, args: &str) -> Value {
    let channel = state.channel();
    let calls = RpiHostCalls { call: channel.call };
    let host = CommandHost {
        calls: &calls,
        cookie: channel.cookie,
    };
    let server = args.trim().to_string();
    if server.is_empty() {
        let text = "Usage: /mcp-auth <server>".to_string();
        host.notify(&text, "error");
        return commands::error_result(text, "invalid_args");
    }
    if !host.has_ui() {
        let text = "OAuth authentication requires an interactive session.".to_string();
        return commands::error_result(text, "no_ui");
    }
    let runtime = match command_runtime(state) {
        Ok(runtime) => runtime,
        Err(result) => return result,
    };
    let Some(definition) = runtime.config.mcp_servers.get(&server).cloned() else {
        let text = format!("Server \"{server}\" not found in config");
        host.notify(&text, "error");
        return commands::error_result(text, "server_not_found");
    };
    if definition.is_disabled() {
        let text =
            format!("Server \"{server}\" is disabled. Run /mcp enable {server}, then /reload.");
        host.notify(&text, "warning");
        return commands::error_result(text, "server_disabled");
    }
    if !crate::manager::supports_oauth(&definition) {
        let text = format!(
            "Server \"{server}\" does not use OAuth authentication.\nSet \"auth\": \"oauth\" or omit auth for auto-detection."
        );
        host.notify(&text, "error");
        return commands::error_result(text, "not_oauth");
    }
    let server_url = match crate::utils::resolve_server_url(definition.get("url")) {
        Ok(Some(url)) => url,
        Ok(None) => {
            let text = format!(
                "Server \"{server}\" has no URL configured (OAuth requires HTTP transport)"
            );
            host.notify(&text, "error");
            return commands::error_result(text, "no_url");
        }
        Err(_) => {
            // The resolution error embeds the interpolated URL (potential
            // credential material) — never forward it (G4).
            let text = format!("Server \"{server}\" has an invalid or unresolvable URL");
            host.notify(&text, "error");
            return commands::error_result(text, "invalid_url");
        }
    };

    // The authorization-URL callback runs on the OAuth task: capture the
    // host trampoline (fn pointer + cookie are Copy and 'static) instead of
    // borrowing the CommandHost.
    let call = channel.call;
    let cookie = channel.cookie;
    let server_for_url = server.clone();
    let options = crate::oauth::AuthenticateOptions {
        auth_storage_options: crate::oauth::store::AuthStorageOptions {
            base_dir: oauth_dir(&runtime),
        },
        on_authorization_url: Some(Arc::new(move |url: &str| {
            let calls = RpiHostCalls { call };
            let message =
                format!("Complete {server_for_url} OAuth\nOpen the authorization page:\n{url}");
            host_call(
                &calls,
                cookie,
                "ui.notify",
                json!({"message": message, "notifyType": "info"}),
            );
        })),
        ..Default::default()
    };

    host.set_status("mcp-auth", Some(&format!("Authenticating {server}...")));
    let outcome = state.runtime.block_on(crate::oauth::authenticate(
        &server,
        &server_url,
        &definition,
        &options,
    ));
    host.set_status("mcp-auth", None);
    match outcome {
        Ok(_) => {
            let text = format!("OAuth authentication successful for \"{server}\".");
            host.notify(&text, "info");
            commands::text_result(text)
        }
        Err(error) => {
            let text = format!("Failed to authenticate \"{server}\": {error}");
            host.notify(&text, "error");
            commands::error_result(text, "auth_failed")
        }
    }
}

/// `settings.oauthDir` → auth store base directory (proxy.rs
/// `attempt_auto_auth` parity).
fn oauth_dir(runtime: &proxy::McpRuntime) -> Option<std::path::PathBuf> {
    runtime
        .config
        .settings
        .as_ref()
        .and_then(|settings| settings.get("oauthDir"))
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
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
