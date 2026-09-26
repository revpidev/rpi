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

use serde_json::{json, Value};

pub mod events;
pub mod interactive_ui;
pub mod model_registry;
pub mod session_entries;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "rpi")]
extern "C" {
    fn rpi_host_call(ptr: *const u8, len: usize) -> u64;
}

// ============================================================================
// ABI plumbing (alloc/dealloc/pack/unpack)
// ============================================================================

/// Host-callable allocator (host writes responses into guest memory).
///
/// # Safety
/// Called by the host with a byte length; the returned region stays valid
/// until `rpi_dealloc`.
#[no_mangle]
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
#[no_mangle]
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
    let packed = unsafe { rpi_host_call(bytes.as_ptr(), bytes.len()) };
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

struct State {
    handlers: Vec<Registration>,
    tools: Vec<ToolRegistration>,
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
        #[no_mangle]
        pub extern "C" fn rpi_extension_init() -> u64 {
            let mut ext = $crate::Extension::new();
            $register(&mut ext);
            $crate::finish_init()
        }

        #[no_mangle]
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
            )
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
            let state = state();
            state
                .tools
                .iter()
                .find(|tool| tool.definition.get("name").and_then(Value::as_str) == Some(tool_name))
                .map(|tool| match &tool.execute {
                    ToolExecute::Simple(execute) => execute(params),
                    ToolExecute::WithContext(execute) => execute(ToolContext {
                        params,
                        tool_call_id: message
                            .get("toolCallId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }),
                })
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
            let id_b = ext.on(&event_for_b, move |_| {
                calls_b.lock().unwrap().push("B");
                Ok(Value::Null)
            });
            id_b
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
}
