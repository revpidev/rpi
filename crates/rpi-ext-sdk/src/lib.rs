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
pub fn host_call(method: &str, args: Value) -> Result<Value, String> {
    let request = json!({
        "call": method,
        "args": args,
        "seq": next_seq(),
    });
    let bytes = serde_json::to_vec(&request).unwrap_or_default();
    let packed = unsafe { rpi_host_call(bytes.as_ptr(), bytes.len()) };
    let ptr = (packed >> 32) as u32 as *mut u8;
    let len = (packed & 0xffff_ffff) as usize;
    let response: Value = serde_json::from_slice(&unpack(ptr, len))
        .map_err(|e| format!("host response JSON: {e}"))?;
    unsafe { rpi_dealloc(ptr, len) };
    match response.get("error") {
        Some(error) => Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("host error")
            .to_owned()),
        None => Ok(response.get("ok").cloned().unwrap_or(Value::Null)),
    }
}

fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// ============================================================================
// Extension builder + dispatch table
// ============================================================================

type Handler = Box<dyn Fn(Value) -> Result<Value, String> + Send>;

struct Registration {
    event: String,
    handler: Handler,
}

struct ToolRegistration {
    definition: Value,
    execute: Handler,
}

struct State {
    handlers: Vec<Registration>,
    tools: Vec<ToolRegistration>,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<State>> = std::sync::OnceLock::new();
    STATE
        .get_or_init(|| {
            std::sync::Mutex::new(State {
                handlers: Vec::new(),
                tools: Vec::new(),
            })
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Extension builder used inside `register`.
pub struct Extension {
    _private: (),
}

impl Extension {
    /// `pi.on(event, handler)`: handler receives the event payload JSON and
    /// returns the result JSON (`Value::Null` = undefined).
    pub fn on(
        &mut self,
        event: &str,
        handler: impl Fn(Value) -> Result<Value, String> + Send + 'static,
    ) -> &mut Self {
        state().handlers.push(Registration {
            event: event.to_owned(),
            handler: Box::new(handler),
        });
        self
    }

    /// `pi.registerTool(definition, execute)`: `definition` carries
    /// name/label/description/parameters (see docs/extension-abi.md).
    pub fn tool(
        &mut self,
        definition: Value,
        execute: impl Fn(Value) -> Result<Value, String> + Send + 'static,
    ) -> &mut Self {
        state().tools.push(ToolRegistration {
            definition,
            execute: Box::new(execute),
        });
        self
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
    {
        let state = state();
        // One host subscription per event; guest-side fan-out happens in
        // `dispatch`.
        let mut events: Vec<&str> = Vec::new();
        for registration in &state.handlers {
            if !events.contains(&registration.event.as_str()) {
                events.push(&registration.event);
            }
        }
        for event in events {
            if let Err(error) = host_call("on", json!({ "event": event })) {
                init_errors.push(error);
            }
        }
        for tool in &state.tools {
            let mut definition = tool.definition.clone();
            if let Value::Object(map) = &mut definition {
                map.insert("renderCall".to_owned(), json!(false));
                map.insert("renderResult".to_owned(), json!(false));
            }
            if let Err(error) = host_call("registerTool", json!({ "definition": definition })) {
                init_errors.push(error);
            }
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
    let message: Value = match serde_json::from_slice(&unpack(ptr as *const u8, len)) {
        Ok(message) => message,
        Err(error) => {
            return pack_json(&json!({"error": {"kind": "invalidRequest", "message": error.to_string()}}))
        }
    };
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("");
    let result = match kind {
        "event" => {
            let event = message
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let payload = message.get("payload").cloned().unwrap_or(Value::Null);
            let state = state();
            // Serial, registration order; the last non-null result wins
            // (mirrors the runner's within-extension chaining).
            let mut result = None;
            for registration in state.handlers.iter().filter(|r| r.event == event) {
                match (registration.handler)(payload.clone()) {
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
                .find(|tool| {
                    tool.definition.get("name").and_then(Value::as_str) == Some(tool_name)
                })
                .map(|tool| (tool.execute)(params))
        }
        _ => None,
    };
    match result {
        Some(Ok(value)) => pack_json(&value),
        Some(Err(error)) => pack_json(&json!({"error": {"kind": "handler", "message": error}})),
        None => pack_json(&Value::Null),
    }
}
