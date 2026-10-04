//! `rpi-ext-sdk` — guest-side SDK for rpi wasm extensions (ABI v1).
//!
//! Build for `wasm32-unknown-unknown` (no WASI):
//!
//! ```sh
//! cargo build --target wasm32-unknown-unknown --release
//! ```
//!
//! An extension defines `register(ext: &mut Extension)` and exports the ABI
//! through [`export!`]. The host calls `rpi_extension_init` once (your
//! registrations run there) and `rpi_dispatch` per event/tool call; guest
//! → host requests go through `rpi_host_call` as JSON (see
//! `docs/extension-abi.md`).

use serde_json::{Value, json};

pub mod events;
pub mod interactive_ui;
pub mod model_registry;
pub mod session_entries;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "rpi")]
unsafe extern "C" {
    // `safe`: the guest owns both buffers; the host import honors the
    // v1 ABI contract (reads `len` bytes at `ptr`, returns a packed
    // ptr/len pair the guest may read).
    safe fn rpi_host_call(ptr: *const u8, len: usize) -> u64;
}

// ============================================================================
// ABI plumbing (alloc/dealloc/pack/unpack)
// ============================================================================

/// Host-callable allocator (host writes responses into guest memory).
///
/// # Safety
/// Called by the host with a byte length; the returned region stays valid
/// until `rpi_dealloc`.
#[unsafe(no_mangle)]
pub extern "C" fn rpi_alloc(len: usize) -> *mut u8 {
    let mut buf: Vec<u8> = Vec::with_capacity(len.max(1));
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Free a region produced by [`rpi_alloc`].
///
/// # Safety
/// `ptr`/`len` must come from `rpi_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rpi_dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

fn pack(bytes: Vec<u8>) -> u64 {
    let ptr = bytes.as_ptr() as u64;
    let len = bytes.len() as u64;
    std::mem::forget(bytes);
    (ptr << 32) | len
}

fn unpack(ptr: *const u8, len: usize) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

fn pack_json(value: &Value) -> u64 {
    pack(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

// ============================================================================
// Host call client
// ============================================================================

/// Raw `rpi_host_call`: `{"call": method, "args": {...}, "seq": N}` →
/// `{"ok": ...} | {"error": {"kind", "message"}}`.
///
/// String-error wrapper over [`host_call_typed`] (pre-v0.1.4 contract kept
/// for existing callers).
#[cfg(target_arch = "wasm32")]
pub fn host_call(method: &str, args: Value) -> Result<Value, String> {
    host_call_typed(method, args).map_err(|error| error.to_string())
}

/// Host call with the structured error kind preserved (V14-20 C0: the
/// interactive UI probe needs `unknownMethod` vs other kinds; `Display` of
/// the error is message-only, so existing string contracts do not change).
#[cfg(target_arch = "wasm32")]
pub fn host_call_typed(
    method: &str,
    args: Value,
) -> Result<Value, interactive_ui::InteractiveUiError> {
    use interactive_ui::InteractiveUiError;
    let request = json!({
        "call": method,
        "args": args,
        "seq": next_seq(),
    });
    let bytes = serde_json::to_vec(&request)
        .map_err(|error| InteractiveUiError::protocol(format!("host request JSON: {error}")))?;
    // `rpi_host_call` is declared `safe` in the extern block: the guest owns
    // both buffers and the host honors the v1 ABI contract.
    let packed = rpi_host_call(bytes.as_ptr(), bytes.len());
    let ptr = (packed >> 32) as u32 as *mut u8;
    let len = (packed & 0xffff_ffff) as usize;
    let response: Value = serde_json::from_slice(&unpack(ptr, len))
        .map_err(|error| InteractiveUiError::protocol(format!("host response JSON: {error}")))?;
    unsafe { rpi_dealloc(ptr, len) };
    match InteractiveUiError::from_envelope(&response) {
        Some(error) => Err(error),
        None => Ok(response.get("ok").cloned().unwrap_or(Value::Null)),
    }
}

/// Host target: the ABI import does not exist, so the guest transport is
/// unavailable. Returning a structured error (instead of a link failure)
/// keeps the crate testable on the host under `cargo test --workspace`.
#[cfg(not(target_arch = "wasm32"))]
pub fn host_call(method: &str, args: Value) -> Result<Value, String> {
    host_call_typed(method, args).map_err(|error| error.to_string())
}

/// Host target stub of [`host_call_typed`] (see the wasm32 version).
#[cfg(not(target_arch = "wasm32"))]
pub fn host_call_typed(
    _method: &str,
    _args: Value,
) -> Result<Value, interactive_ui::InteractiveUiError> {
    Err(interactive_ui::InteractiveUiError::protocol(
        "rpi host calls are only available on wasm32 guests",
    ))
}

#[cfg(target_arch = "wasm32")]
fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// ============================================================================
// Extension builder + dispatch table
// ============================================================================

type Handler = std::sync::Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>;

struct Registration {
    /// Guest-local registration id — the `pi.on()` unsubscribe handle
    /// (#8967, 46c9de402; upstream returns a closure, ids are the ABI
    /// equivalent, see `unsubscribe`).
    id: u64,
    event: String,
    handler: Handler,
}

/// Host `on` subscription ids per event name: the wasm carrier dedupes to
/// ONE host-side forwarder per event (guest-side fan-out happens in
/// `dispatch`), so the host subscription is only torn down when the last
/// guest handler for that event goes away.
struct HostSubscriptions(std::collections::HashMap<String, u64>);

/// Context handed to tools registered via [`Extension::tool_with_context`]
/// (ADR-0015): the tool-call params plus the `toolCallId`, which backs
/// [`ToolContext::report_update`].
pub struct ToolContext {
    /// The `params` of the `toolExecute` dispatch.
    pub params: Value,
    /// The `toolCallId` of the in-flight call.
    pub tool_call_id: String,
}

impl ToolContext {
    /// `on_update` report (ADR-0015): stream a partial `AgentToolResult`
    /// (`{"content": [...], "details": ...}`) to the host while the tool is
    /// executing. Fire-and-forget; updates after execute returns are dropped
    /// by the host.
    pub fn report_update(&self, update: Value) -> Result<(), String> {
        host_call(
            "toolUpdate",
            json!({"toolCallId": self.tool_call_id, "update": update}),
        )
        .map(|_| ())
    }

    /// `ctx.executeTool(name, args)` (V16-06 FR-E): run another tool through
    /// the host's full tool pipeline. Only available while this tool
    /// executes; tool failures come back inside the outcome (`isError`),
    /// while transport/capability failures return `Err`. `onUpdate`/`signal`
    /// are native-carrier options and are not portable over the JSON ABI.
    pub fn execute_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        host_call("executeTool", json!({"name": name, "args": args}))
    }
}

type ContextToolHandler =
    std::sync::Arc<dyn Fn(ToolContext) -> Result<Value, String> + Send + Sync>;

enum ToolExecute {
    Simple(Handler),
    WithContext(ContextToolHandler),
}

struct ToolRegistration {
    definition: Value,
    execute: ToolExecute,
}

/// A virtual-model registration (V16-12): the catalog definition is sent to
/// the host on init; the route handler stays guest-side and answers the
/// host→guest `virtualModelRoute` dispatch.
struct VirtualModelRegistration {
    provider: String,
    id: String,
    definition: Value,
    route: std::sync::Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>,
}

struct State {
    handlers: Vec<Registration>,
    tools: Vec<ToolRegistration>,
    virtual_models: Vec<VirtualModelRegistration>,
    next_handler_id: u64,
    host_subscriptions: HostSubscriptions,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<State>> = std::sync::OnceLock::new();
    STATE
        .get_or_init(|| {
            std::sync::Mutex::new(State {
                handlers: Vec::new(),
                tools: Vec::new(),
                virtual_models: Vec::new(),
                next_handler_id: 0,
                host_subscriptions: HostSubscriptions(std::collections::HashMap::new()),
            })
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// `pi.on()` unsubscribe handle (#8967, 46c9de402): pass it to
/// [`unsubscribe`] to drop exactly that registration. Copyable on purpose —
/// upstream's closure can be called from any handler that captured it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionId(u64);

/// Unsubscribe one registration (#8967). Idempotent — a second call with
/// the same id (or an id whose registration already went away) is a silent
/// no-op, mirroring the upstream `indexOf === -1` guard. When the last
/// guest handler of an event is removed, the host-side forwarder is torn
/// down via the additive `off` host-call (best effort — hosts that predate
/// the method answer `unknownMethod`, which is ignored).
pub fn unsubscribe(id: SubscriptionId) {
    let orphaned_event = {
        let mut state = state();
        let Some(index) = state.handlers.iter().position(|r| r.id == id.0) else {
            return;
        };
        let event = state.handlers[index].event.clone();
        state.handlers.remove(index);
        // Host teardown only when the LAST guest handler for the event went
        // away — the host-side forwarder dispatches the whole fan-out list.
        let still_has = state.handlers.iter().any(|r| r.event == event);
        (!still_has).then_some(event)
    };
    if let Some(event) = orphaned_event {
        // The state lock is RELEASED before the host call (the if-let
        // scrutinee's temporary guard would otherwise live through the
        // body — the same-thread re-lock invariant `host_on` documents
        // applies to `off` identically; a host that re-enters the guest
        // while answering must not deadlock on this lock).
        let subscription_id = {
            let mut state = state();
            state.host_subscriptions.0.remove(&event)
        };
        if let Some(subscription_id) = subscription_id {
            let _ = host_call("off", json!({ "subscriptionId": subscription_id }));
        }
    }
}

/// `pi.on(event, handler)` at runtime — post-init subscription usable from
/// handlers, commands, and tools (upstream's `pi.on` is callable at any
/// time; the [`Extension`] builder is only in scope during `register`).
/// Mirrors the builder form: registers the guest handler, establishes the
/// host forwarder only when the event has none yet.
pub fn subscribe(
    event: &str,
    handler: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
) -> SubscriptionId {
    let handler: Handler = std::sync::Arc::new(handler);
    let (id, needs_host_on) = {
        let mut state = state();
        let id = state.next_handler_id;
        state.next_handler_id += 1;
        let needs_host_on = !state.host_subscriptions.0.contains_key(event);
        state.handlers.push(Registration {
            id,
            event: event.to_owned(),
            handler,
        });
        (id, needs_host_on)
    };
    if needs_host_on {
        // Outside the state lock (`host_on` must not re-enter it).
        if let Some(subscription_id) = host_on(event) {
            state()
                .host_subscriptions
                .0
                .insert(event.to_owned(), subscription_id);
        }
    }
    SubscriptionId(id)
}

/// Establish the host-side forwarder for `event`; returns the host
/// subscription id when the host answers one. Old hosts (pre-V15-09)
/// answer `null` — the caller records nothing and teardown degrades to a
/// no-op. MUST be called without the state lock held (same-thread
/// re-locking would deadlock).
fn host_on(event: &str) -> Option<u64> {
    host_call("on", json!({ "event": event }))
        .ok()
        .and_then(|response| response.get("subscriptionId").and_then(Value::as_u64))
}

/// Extension builder used inside `register`.
pub struct Extension {
    _private: (),
}

impl Extension {
    /// `pi.on(event, handler)`: handler receives the event payload JSON and
    /// returns the result JSON (`Value::Null` = undefined). Returns the
    /// unsubscribe handle (#8967, 46c9de402) — pass it to [`unsubscribe`].
    pub fn on(
        &mut self,
        event: &str,
        handler: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> SubscriptionId {
        let id = {
            let mut state = state();
            let id = state.next_handler_id;
            state.next_handler_id += 1;
            state.handlers.push(Registration {
                id,
                event: event.to_owned(),
                handler: std::sync::Arc::new(handler),
            });
            id
        };
        SubscriptionId(id)
    }

    /// `pi.registerTool(definition, execute)`: `definition` carries
    /// name/label/description/parameters (see docs/extension-abi.md).
    pub fn tool(
        &mut self,
        definition: Value,
        execute: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> &mut Self {
        state().tools.push(ToolRegistration {
            definition,
            execute: ToolExecute::Simple(std::sync::Arc::new(execute)),
        });
        self
    }

    /// Like [`Extension::tool`], but the handler receives a [`ToolContext`]
    /// and can stream partial results via [`ToolContext::report_update`]
    /// (ADR-0015).
    pub fn tool_with_context(
        &mut self,
        definition: Value,
        execute: impl Fn(ToolContext) -> Result<Value, String> + Send + Sync + 'static,
    ) -> &mut Self {
        state().tools.push(ToolRegistration {
            definition,
            execute: ToolExecute::WithContext(std::sync::Arc::new(execute)),
        });
        self
    }

    /// `pi.unregisterTool(name)` (ADR-0015): remove a tool this extension
    /// registered. Returns `true` when a registration was removed; unknown
    /// names return `false` (no error).
    pub fn unregister_tool(&self, name: &str) -> Result<bool, String> {
        host_call("unregisterTool", json!({ "name": name }))
            .map(|value| value.as_bool().unwrap_or(false))
    }

    /// `pi.registerVirtualModel(model)` (V16-12, R3.11;
    /// `virtual-models.ts:84-102`): register a virtual catalog entry whose
    /// `route` handler picks a physical model + thinking level per request.
    /// `definition` is the `VirtualModelDefinition` JSON
    /// (`provider`/`id`/`name`/`thinkingLevels?`/`contextWindow?`/
    /// `maxTokens?`/`input?`); `route` receives the `ModelRouteRequest`
    /// JSON and returns the `ModelRoute` JSON
    /// (`{model: {provider, id}, thinkingLevel, state?}`). The host stores
    /// the state entry on the session branch; return `request.state` or
    /// omit `state` to keep it.
    pub fn virtual_model(
        &mut self,
        definition: Value,
        route: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> &mut Self {
        let provider = definition
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let id = definition
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        state().virtual_models.push(VirtualModelRegistration {
            provider,
            id,
            definition,
            route: std::sync::Arc::new(route),
        });
        self
    }

    /// `pi.unregisterVirtualModel(provider, id)` (V16-12): remove a virtual
    /// model this extension registered; unknown provider/id pairs are a
    /// silent no-op.
    pub fn unregister_virtual_model(&self, provider: &str, id: &str) -> Result<(), String> {
        state()
            .virtual_models
            .retain(|entry| entry.provider != provider || entry.id != id);
        host_call(
            "unregisterVirtualModel",
            json!({ "provider": provider, "id": id }),
        )
        .map(|_| ())
    }

    /// `pi.registerMcpServer(name, config)` (V16-08 FR-E): register an MCP
    /// server this extension provides. The registration is not persisted;
    /// register again on every load. A server of the same name in `mcp.json`
    /// takes precedence.
    pub fn register_mcp_server(&self, name: &str, config: Value) -> Result<(), String> {
        host_call(
            "registerMcpServer",
            json!({ "name": name, "config": config }),
        )
        .map(|_| ())
    }

    /// `pi.unregisterMcpServer(name)` (V16-08 FR-E): remove a server this
    /// extension registered and close its connection.
    pub fn unregister_mcp_server(&self, name: &str) -> Result<(), String> {
        host_call("unregisterMcpServer", json!({ "name": name })).map(|_| ())
    }

    /// `pi.getMcpServers()` (V16-08 FR-E): every MCP server registered by
    /// extensions, in registration order.
    pub fn get_mcp_servers(&self) -> Result<Vec<Value>, String> {
        host_call("getMcpServers", json!({}))
            .map(|value| value.as_array().cloned().unwrap_or_default())
    }

    /// `pi.getMode()` (V16-05 FR-B R4; rpi-own): the session permission
    /// mode wire value (`"default"` / `"plan"`).
    pub fn get_mode(&self) -> Result<String, String> {
        host_call("getMode", json!({})).and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "getMode: expected a string".to_owned())
        })
    }

    /// `pi.setMode(mode)` (V16-05 FR-B R4; rpi-own): set the session
    /// permission mode. Unknown values are ignored by the host.
    pub fn set_mode(&self, mode: &str) -> Result<(), String> {
        host_call("setMode", json!({ "mode": mode })).map(|_| ())
    }

    // -- V16-05 usage-provider framework (rpi-own, FR-A R5) ----------------

    /// `ctx.usage.listProviders()`: provider ids reachable through the
    /// explicit settings map, the user script directory, or plugin
    /// registration.
    pub fn usage_list_providers(&self) -> Result<Vec<String>, String> {
        host_call("ctx.usage.listProviders", json!({})).map(|value| {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// `ctx.usage.fetch(provider, force?)`: the latest successful usage
    /// envelope JSON, or `None` when no fetch ever succeeded. Fresh-cache
    /// hits return without running a script; failures keep the last
    /// success.
    pub fn usage_fetch(&self, provider: &str, force: bool) -> Result<Option<Value>, String> {
        host_call(
            "ctx.usage.fetch",
            json!({ "provider": provider, "force": force }),
        )
        .map(|value| if value.is_null() { None } else { Some(value) })
    }

    /// `ctx.usage.register(provider, scriptPath)`: register (or replace) a
    /// provider's script path. Pre-bind calls queue in the extension API
    /// and flush on `bindCore`.
    pub fn usage_register(&self, provider: &str, script_path: &str) -> Result<(), String> {
        host_call(
            "ctx.usage.register",
            json!({ "provider": provider, "scriptPath": script_path }),
        )
        .map(|_| ())
    }

    /// Whether the host implements the V16-05 permission-mode surface
    /// (`getMode` / `setMode` / `mode_change`). Probes the read-only
    /// `getMode`; an older host answers `unknownMethod` (`Ok(false)`).
    pub fn supports_permission_mode(&self) -> Result<bool, String> {
        match host_call_typed("getMode", json!({})) {
            Ok(_) => Ok(true),
            Err(error) => match error.kind {
                crate::interactive_ui::InteractiveUiErrorKind::UnknownMethod => Ok(false),
                _ => Err(error.to_string()),
            },
        }
    }

    /// Whether the host implements the V16-05 usage-provider surface
    /// (`ctx.usage.*`). Probes the read-only `ctx.usage.listProviders`; an
    /// older host answers `unknownMethod` (`Ok(false)`).
    pub fn supports_usage_providers(&self) -> Result<bool, String> {
        match host_call_typed("ctx.usage.listProviders", json!({})) {
            Ok(_) => Ok(true),
            Err(error) => match error.kind {
                crate::interactive_ui::InteractiveUiErrorKind::UnknownMethod => Ok(false),
                _ => Err(error.to_string()),
            },
        }
    }

    /// Host call escape hatch for the rest of the capability surface
    /// (ui.*/ctx.*/command.*/provider/exec — docs/extension-abi.md).
    pub fn call(&self, method: &str, args: Value) -> Result<Value, String> {
        host_call(method, args)
    }
}

/// Define an extension: `export!(my_extension);` where
/// `fn my_extension(ext: &mut Extension)` performs registrations.
#[macro_export]
macro_rules! export {
    ($register:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn rpi_extension_init() -> u64 {
            let mut ext = $crate::Extension::new();
            $register(&mut ext);
            $crate::finish_init()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn rpi_dispatch(ptr: u32, len: usize) -> u64 {
            $crate::dispatch(ptr, len)
        }
    };
}

impl Extension {
    #[doc(hidden)]
    pub fn new() -> Self {
        Extension { _private: () }
    }
}

impl Default for Extension {
    fn default() -> Self {
        Self::new()
    }
}

/// `rpi_extension_init` tail: push registrations to the host, then return
/// the receipt (`{"ok": true}` or `{"error": {...}}`).
#[doc(hidden)]
pub fn finish_init() -> u64 {
    let mut init_errors = Vec::new();
    let events: Vec<String> = {
        let state = state();
        // One host subscription per event that still has handlers (guest-
        // side fan-out happens in `dispatch`); handlers fully unsubscribed
        // during `register` never reach the host.
        let mut events: Vec<String> = Vec::new();
        for registration in &state.handlers {
            if !events.contains(&registration.event) {
                events.push(registration.event.clone());
            }
        }
        events
    };
    for event in &events {
        // #8967 (V15-09): the host answers `{subscriptionId}` — recorded
        // for later `off` teardown. Pre-V15-09 hosts answer `null`.
        if let Some(subscription_id) = host_on(event) {
            state()
                .host_subscriptions
                .0
                .insert(event.clone(), subscription_id);
        }
    }
    let tools: Vec<Value> = {
        let state = state();
        state
            .tools
            .iter()
            .map(|tool| {
                let mut definition = tool.definition.clone();
                if let Value::Object(map) = &mut definition {
                    map.insert("renderCall".to_owned(), json!(false));
                    map.insert("renderResult".to_owned(), json!(false));
                }
                definition
            })
            .collect()
    };
    for definition in tools {
        if let Err(error) = host_call("registerTool", json!({ "definition": definition })) {
            init_errors.push(error);
        }
    }
    // V16-12: virtual-model definitions register on init; the route
    // handlers stay guest-side for the `virtualModelRoute` dispatch.
    let virtual_models: Vec<Value> = {
        let state = state();
        state
            .virtual_models
            .iter()
            .map(|registration| registration.definition.clone())
            .collect()
    };
    for definition in virtual_models {
        if let Err(error) = host_call("registerVirtualModel", json!({ "definition": definition })) {
            init_errors.push(error);
        }
    }
    if init_errors.is_empty() {
        pack_json(&json!({"ok": true}))
    } else {
        pack_json(&json!({"error": {"kind": "init", "message": init_errors.join("; ")}}))
    }
}

/// `rpi_dispatch` entry: route the message to the registered handler.
#[doc(hidden)]
pub fn dispatch(ptr: u32, len: usize) -> u64 {
    dispatch_value(unpack(ptr as *const u8, len))
}

/// `rpi_dispatch` body over an owned message buffer (the raw entry's `u32`
/// pointer is a wasm32 linear-memory offset; host-target tests drive
/// [`route`] instead — the `pack`/`unpack` pointer packing assumes <4 GiB
/// guest addresses and is meaningless on 64-bit hosts).
#[doc(hidden)]
pub fn dispatch_value(message: Vec<u8>) -> u64 {
    let message: Value = match serde_json::from_slice(&message) {
        Ok(message) => message,
        Err(error) => {
            return pack_json(
                &json!({"error": {"kind": "invalidRequest", "message": error.to_string()}}),
            );
        }
    };
    match route(message) {
        Some(Ok(value)) => pack_json(&value),
        Some(Err(error)) => pack_json(&json!({"error": {"kind": "handler", "message": error}})),
        None => pack_json(&Value::Null),
    }
}

/// Routing core of `rpi_dispatch` over the parsed message. Host-target
/// tests drive this directly.
#[doc(hidden)]
pub fn route(message: Value) -> Option<Result<Value, String>> {
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "event" => {
            let event = message
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let payload = message.get("payload").cloned().unwrap_or(Value::Null);
            // Snapshot semantics (#8967, 46c9de402): handlers added or
            // removed during this dispatch apply to LATER dispatches — the
            // fan-out list is cloned (Arc clones) and run outside the state
            // lock, so a handler may call `subscribe`/`unsubscribe` freely.
            let handlers: Vec<Handler> = {
                let state = state();
                state
                    .handlers
                    .iter()
                    .filter(|r| r.event == event)
                    .map(|r| r.handler.clone())
                    .collect()
            };
            // Serial, registration order; the last non-null result wins
            // (mirrors the runner's within-extension chaining).
            let mut result = None;
            for handler in handlers {
                match handler(payload.clone()) {
                    Ok(value) if !value.is_null() => result = Some(Ok(value)),
                    Ok(_) => {}
                    Err(error) => result = Some(Err(error)),
                }
            }
            result
        }
        "toolExecute" => {
            let tool_name = message
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let tool_call_id = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            // Snapshot the handler under the lock and RELEASE it before
            // invoking: a tool handler must be able to call `subscribe` /
            // `unsubscribe` (which re-lock the same non-reentrant state
            // mutex) without deadlocking — the event arm snapshots for the
            // same reason.
            let execute = {
                let state = state();
                let tool = state.tools.iter().find(|tool| {
                    tool.definition.get("name").and_then(Value::as_str) == Some(tool_name)
                })?;
                match &tool.execute {
                    ToolExecute::Simple(execute) => ToolExecute::Simple(execute.clone()),
                    ToolExecute::WithContext(execute) => ToolExecute::WithContext(execute.clone()),
                }
            };
            match execute {
                ToolExecute::Simple(execute) => Some(execute(params)),
                ToolExecute::WithContext(execute) => Some(execute(ToolContext {
                    params,
                    tool_call_id,
                })),
            }
        }
        // V16-12: host→guest route call for a registered virtual model. The
        // request names the virtual model selection, which identifies the
        // handler; the guest-side state lock is released before invoking it
        // (the handler may host-call).
        "virtualModelRoute" => {
            let request = message.get("request").cloned().unwrap_or(Value::Null);
            let provider = request
                .get("model")
                .and_then(|model| model.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = request
                .get("model")
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let route = {
                let state = state();
                state
                    .virtual_models
                    .iter()
                    .find(|entry| entry.provider == provider && entry.id == id)
                    .map(|entry| entry.route.clone())
            };
            route.map(|route| route(request))
        }
        _ => None,
    }
}

// ============================================================================
// Tests (#8967 snapshot semantics; host target — `host_on` degrades to a
// no-op there, so these cover the guest-side fan-out contract)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TOKEN: AtomicU64 = AtomicU64::new(0);

    /// Drive one host→guest dispatch through the routing core (`route`) —
    /// the raw entry's u32 pointer packing is wasm32-only.
    fn dispatch_message(message: &Value) -> Value {
        match route(message.clone()) {
            Some(Ok(value)) => value,
            Some(Err(error)) => json!({"error": {"kind": "handler", "message": error}}),
            None => Value::Null,
        }
    }

    /// Unique event name per test (the SDK state is process-global).
    fn unique_event(prefix: &str) -> String {
        format!("{}_{}", prefix, TOKEN.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn on_returns_distinct_subscription_ids() {
        let event = unique_event("ids");
        let mut ext = Extension::new();
        let first = ext.on(&event, |_| Ok(Value::Null));
        let second = ext.on(&event, |_| Ok(Value::Null));
        assert_ne!(first, second);
    }

    #[test]
    fn unsubscribe_removes_handler_from_fanout() {
        let event = unique_event("unsub");
        let mut ext = Extension::new();
        let id = ext.on(&event, |_| Ok(json!("gone")));
        ext.on(&event, |_| Ok(json!("kept")));

        let message = json!({ "kind": "event", "event": event, "payload": null });
        let reply = dispatch_message(&message);
        assert_eq!(
            reply,
            json!("kept"),
            "last non-null result wins with both handlers"
        );

        unsubscribe(id);
        let reply = dispatch_message(&message);
        assert_eq!(reply, json!("kept"), "removed handler no longer runs");
    }

    #[test]
    fn unsubscribe_unknown_id_is_silent_noop() {
        unsubscribe(SubscriptionId(9_999_999));
    }

    /// Review P2: the `toolExecute` dispatch must release the SDK state lock
    /// before running the handler — `subscribe`/`unsubscribe` re-lock it, so
    /// a handler that subscribes used to deadlock the guest thread.
    #[test]
    fn tool_handler_can_subscribe_without_deadlocking() {
        let event = unique_event("tool-sub");
        let mut ext = Extension::new();
        ext.tool_with_context(
            json!({
                "name": "echo",
                "label": "Echo",
                "description": "echo params",
                "parameters": {"type": "object"}
            }),
            move |ctx| {
                let subscription = subscribe(&event, |_| Ok(Value::Null));
                unsubscribe(subscription);
                Ok(ctx.params)
            },
        );
        let message = json!({
            "kind": "toolExecute",
            "toolName": "echo",
            "params": {"hello": "world"},
            "toolCallId": "call-1"
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(route(message));
        });
        let reply = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("tool handler must not deadlock on the SDK state lock");
        assert_eq!(
            reply.expect("toolExecute routed").expect("handler ok"),
            json!({"hello": "world"})
        );
    }

    /// Snapshot semantics: removals during a dispatch keep the removed
    /// handler in the CURRENT dispatch; registrations during a dispatch are
    /// deferred to the next one (upstream "keeps removed pending handlers"
    /// + "defers registrations", extensions-runner.test.ts @ 46c9de402).
    #[test]
    fn dispatch_snapshot_semantics_for_add_and_remove() {
        let event = std::sync::Arc::new(unique_event("snapshot"));
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

        let mut ext = Extension::new();
        let event_for_a = event.to_string();
        let event_for_b = event.to_string();
        let id_b = {
            let calls_a = calls.clone();
            let event_in_a = event_for_a.clone();
            let _ = ext.on(&event_for_a, move |_| {
                calls_a.lock().unwrap().push("A");
                // A removes B (kept in this dispatch) and registers C
                // (deferred to the next dispatch).
                let id_b = ID_SLOT.with(|slot| slot.borrow().unwrap());
                unsubscribe(id_b);
                subscribe(&event_in_a, |_| {
                    REGISTERED_WITH.with(|flag| flag.set(true));
                    Ok(Value::Null)
                });
                Ok(Value::Null)
            });
            let calls_b = calls.clone();

            ext.on(&event_for_b, move |_| {
                calls_b.lock().unwrap().push("B");
                Ok(Value::Null)
            })
        };
        ID_SLOT.with(|slot| *slot.borrow_mut() = Some(id_b));

        let message = json!({ "kind": "event", "event": event.as_str(), "payload": null });
        dispatch_message(&message);
        assert_eq!(*calls.lock().unwrap(), vec!["A", "B"]);
        // Registration made during the dispatch runs on the NEXT dispatch
        // (and B stays removed).
        dispatch_message(&message);
        assert_eq!(*calls.lock().unwrap(), vec!["A", "B", "A"]);
        assert!(REGISTERED_WITH.with(|flag| flag.get()));
    }

    thread_local! {
        static ID_SLOT: std::cell::RefCell<Option<SubscriptionId>> =
            const { std::cell::RefCell::new(None) };
        static REGISTERED_WITH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// V16-12: a registered virtual model answers the host→guest
    /// `virtualModelRoute` dispatch by provider/id; unknown selections are
    /// a silent `null` (the host surfaces them as a routing error).
    #[test]
    fn virtual_model_dispatch_routes_to_the_registered_handler() {
        let mut ext = Extension::new();
        ext.virtual_model(
            json!({"provider": "router", "id": "auto", "name": "Auto"}),
            |request| {
                Ok(json!({
                    "model": {"provider": "faux", "id": request["reason"]},
                    "thinkingLevel": "medium",
                }))
            },
        );
        let reply = dispatch_message(&json!({
            "kind": "virtualModelRoute",
            "request": {
                "model": {"provider": "router", "id": "auto"},
                "reason": "user",
            },
        }));
        assert_eq!(reply["model"]["id"], "user");
        assert_eq!(reply["thinkingLevel"], "medium");

        let missing = dispatch_message(&json!({
            "kind": "virtualModelRoute",
            "request": {"model": {"provider": "router", "id": "other"}},
        }));
        assert!(missing.is_null());
    }
}
