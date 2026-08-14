//! Wasm (L1) extension runtime — ABI v1 host implementation (T15 W6).
//!
//! ABI v1 (docs/extension-abi.md; design §13 open item 1):
//! - guest exports: `memory`, `rpi_alloc(len: u32) -> u32`,
//!   `rpi_dealloc(ptr: u32, len: u32)`, `rpi_extension_init() -> u64`,
//!   `rpi_dispatch(ptr: u32, len: u32) -> u64`;
//! - host import (module `rpi`): `rpi_host_call(ptr: u32, len: u32) -> u64`;
//! - payloads are UTF-8 JSON in guest linear memory; guest→host pass
//!   `(ptr, len)`; host→guest return `u64 = (ptr << 32) | len` allocated via
//!   the guest's `rpi_alloc`.
//!
//! Threading: every guest instance gets its own `Store` on a dedicated
//! blocking thread; event dispatch is serial per extension by construction
//! (mirrors upstream's serial handler semantics). `rpi_host_call` handlers
//! that need async host work (exec/setModel/dialogs) spawn onto the
//! ambient tokio runtime and block the guest thread on a std channel.
//!
//! Sandbox: no WASI is linked (guests get zero filesystem/network
//! capability by construction), every host call is checked against the
//! manifest capabilities, and fuel metering caps runaway guests
//! (trap → load/dispatch error, never a panic).

pub mod host_call;
pub mod ui_dispatch;

use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

use crate::api::ExtensionApi;

/// ABI version this host implements (`rpiAbi` manifest field).
pub const RPI_ABI_VERSION: u32 = 1;

/// Fuel granted per guest call (init / dispatch / host-call re-entry).
/// Generous for legitimate work, fatal to dead loops (coding-standards
/// §11.4 sandbox intent).
const CALL_FUEL: u64 = 50_000_000;

/// Upper bound for a single guest payload (host-call request or return):
/// guest-controlled lengths are bounds-checked against both this cap and
/// the linear memory BEFORE any host-side allocation — a hostile guest
/// must not trigger a multi-GiB zeroed allocation that its fuel never pays
/// for.
const MAX_HOST_CALL_BYTES: usize = 16 * 1024 * 1024;

/// Timeout for blocking renderer round-trips ([`WasmForward::dispatch_blocking`],
/// TUI-thread renders): a guest blocked in a dialog host-call
/// (`ui.select` etc.) must not freeze the TUI thread — on timeout the
/// render falls back to the built-in rendering instead of deadlocking.
const RENDER_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Grace period for [`WasmGuest::drop`] to join the guest thread before
/// detaching it (a thread stuck in a host call must not hang session
/// replacement / shutdown).
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Extension capabilities (`manifest.capabilities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Tools,
    Commands,
    Ui,
    Session,
    Exec,
    Provider,
    Events,
}

impl Capability {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "tools" => Capability::Tools,
            "commands" => Capability::Commands,
            "ui" => Capability::Ui,
            "session" => Capability::Session,
            "exec" => Capability::Exec,
            "provider" => Capability::Provider,
            "events" => Capability::Events,
            _ => return None,
        })
    }
}

/// Shared engine (module compilation cache lives on the Engine).
fn engine() -> wasmtime::Engine {
    use std::sync::OnceLock;
    static ENGINE: OnceLock<wasmtime::Engine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = wasmtime::Config::new();
            config.consume_fuel(true);
            wasmtime::Engine::new(&config).expect("wasmtime engine config is valid")
        })
        .clone()
}

/// Compile guest bytes (WAT or wasm binary) into a module.
pub fn compile_module(bytes: &[u8]) -> Result<wasmtime::Module, String> {
    wasmtime::Module::new(&engine(), bytes).map_err(|e| e.to_string())
}

/// Commands onto a guest's dedicated thread. Responders are std channels:
/// the async paths wrap `recv` in `spawn_blocking` and the blocking render
/// path uses `recv_timeout` (a tokio oneshot has no timed blocking recv).
pub(crate) enum GuestCommand {
    /// Run `rpi_extension_init`; respond with the receipt JSON.
    Init(std::sync::mpsc::Sender<Result<Value, String>>),
    /// Run `rpi_dispatch` with a JSON message; respond with its result.
    /// `command_context` marks command-handler dispatches (`command.*`
    /// host calls are only legal inside one).
    Dispatch {
        message: Vec<u8>,
        command_context: bool,
        respond: std::sync::mpsc::Sender<Result<Value, String>>,
    },
    /// Drop the store and exit the thread.
    Shutdown,
}

/// Dispatch target for handler forwarding: a wasm guest thread (L1) or a
/// native plugin's exported function (L0 dynamic library, abi_stable).
#[derive(Clone)]
pub enum DispatchTarget {
    Wasm(WasmForward),
    Native(NativeForward),
}

impl DispatchTarget {
    /// `rpi_dispatch`-equivalent round trip (serial per extension).
    pub async fn dispatch(&self, message: Value, command_context: bool) -> Result<Value, String> {
        match self {
            DispatchTarget::Wasm(forward) => forward.dispatch(message, command_context).await,
            DispatchTarget::Native(forward) => forward.dispatch(message, command_context),
        }
    }

    /// Synchronous dispatch for sync host callbacks (render closures).
    pub fn dispatch_blocking(
        &self,
        message: Value,
        command_context: bool,
    ) -> Result<Value, String> {
        match self {
            DispatchTarget::Wasm(forward) => forward.dispatch_blocking(message, command_context),
            DispatchTarget::Native(forward) => forward.dispatch(message, command_context),
        }
    }

    /// Fire-and-forget dispatch (event bus fan-out).
    pub fn dispatch_forget(&self, message: Value) {
        match self {
            DispatchTarget::Wasm(forward) => forward.dispatch_forget(message),
            DispatchTarget::Native(forward) => {
                let _ = forward.dispatch(message, false);
            }
        }
    }
}

/// Native plugin dispatch (abi_stable exported function + context cookie).
/// In-process: the call runs synchronously on the caller — serial per
/// extension because the runner core is serial.
#[derive(Clone)]
pub struct NativeForward {
    pub dispatch_fn: extern "C" fn(
        crate::native::PluginCookie,
        abi_stable::std_types::RVec<u8>,
    ) -> abi_stable::std_types::RVec<u8>,
    /// The cookie as an integer (Send-safe storage); cast back to the
    /// pointer at the call.
    pub cookie: usize,
}

impl NativeForward {
    pub fn dispatch(&self, message: Value, command_context: bool) -> Result<Value, String> {
        let bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        // Command-context rides the calling thread's flag (the plugin may
        // re-enter `rpi_host_call` from any of its own threads; a shared
        // atomic would race with concurrent dispatches).
        struct CommandGuard(bool);
        impl Drop for CommandGuard {
            fn drop(&mut self) {
                crate::native::with_in_command(|cell| cell.set(self.0));
            }
        }
        let previous = crate::native::with_in_command(|cell| cell.replace(command_context));
        let _guard = CommandGuard(previous);
        let response = (self.dispatch_fn)(
            self.cookie as crate::native::PluginCookie,
            abi_stable::std_types::RVec::from(bytes),
        );
        serde_json::from_slice(&response[..])
            .map_err(|e| format!("plugin returned invalid JSON: {e}"))
    }
}

/// Cloneable forward handle into a guest's dispatch loop. Handlers
/// registered on behalf of the guest capture this.
#[derive(Clone)]
pub struct WasmForward {
    tx: std::sync::mpsc::Sender<GuestCommand>,
}

impl WasmForward {
    /// `rpi_dispatch` round trip (serial per guest).
    pub async fn dispatch(&self, message: Value, command_context: bool) -> Result<Value, String> {
        let bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx
            .send(GuestCommand::Dispatch {
                message: bytes,
                command_context,
                respond: tx,
            })
            .map_err(|_| "guest thread is gone".to_owned())?;
        tokio::task::spawn_blocking(move || rx.recv())
            .await
            .map_err(|_| "guest thread dropped the response".to_owned())?
            .map_err(|_| "guest thread dropped the response".to_owned())?
    }

    /// Synchronous dispatch for sync host callbacks (render closures).
    /// Only valid OFF the async runtime threads (render paths run on the
    /// TUI/loader side). Bounded by [`RENDER_DISPATCH_TIMEOUT`]: a guest
    /// blocked in a dialog host-call falls back to the caller's error path
    /// (default rendering) instead of freezing the TUI thread.
    pub fn dispatch_blocking(
        &self,
        message: Value,
        command_context: bool,
    ) -> Result<Value, String> {
        let bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx
            .send(GuestCommand::Dispatch {
                message: bytes,
                command_context,
                respond: tx,
            })
            .map_err(|_| "guest thread is gone".to_owned())?;
        rx.recv_timeout(RENDER_DISPATCH_TIMEOUT)
            .map_err(|_| "render dispatch timed out (guest busy)".to_owned())?
    }

    /// Fire-and-forget dispatch (event bus fan-out).
    pub fn dispatch_forget(&self, message: Value) {
        if let Ok(bytes) = serde_json::to_vec(&message) {
            let (tx, _rx) = std::sync::mpsc::channel();
            let _ = self.tx.send(GuestCommand::Dispatch {
                message: bytes,
                command_context: false,
                respond: tx,
            });
        }
    }
}

/// A live guest instance on its own thread.
pub struct WasmGuest {
    forward: WasmForward,
    join: Option<std::thread::JoinHandle<()>>,
    /// Set by the guest thread right before it exits; lets [`Drop`]
    /// wait with a bounded grace period instead of joining a thread
    /// stuck in a host call. `Condvar` (not an mpsc receiver) because the
    /// guest lives inside a `RwLock<Option<WasmGuest>>`.
    exited: Option<Arc<ExitSignal>>,
}

/// Thread-exit latch shared between the guest thread and [`WasmGuest::drop`].
struct ExitSignal {
    done: std::sync::Mutex<bool>,
    condvar: std::sync::Condvar,
}

impl ExitSignal {
    fn new() -> Arc<Self> {
        Arc::new(ExitSignal {
            done: std::sync::Mutex::new(false),
            condvar: std::sync::Condvar::new(),
        })
    }

    /// Guest thread tail: mark exit and wake the dropper.
    fn signal_exit(&self) {
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.condvar.notify_one();
    }

    /// `true` when the thread exited within the grace period.
    fn wait_exit(&self, grace: std::time::Duration) -> bool {
        let (guard, timeout) = self
            .condvar
            .wait_timeout(self.done.lock().unwrap_or_else(|e| e.into_inner()), grace)
            .unwrap_or_else(|e| e.into_inner());
        *guard || !timeout.timed_out()
    }
}

impl WasmGuest {
    pub fn forward(&self) -> WasmForward {
        self.forward.clone()
    }

    /// Drop the guest thread (best-effort shutdown).
    pub fn shutdown(&self) {
        let _ = self.forward.tx.send(GuestCommand::Shutdown);
    }
}

impl Drop for WasmGuest {
    fn drop(&mut self) {
        self.shutdown();
        // Bounded join: a guest blocked in a host call (e.g. an unanswered
        // dialog) must not hang session replacement / shutdown. Within the
        // grace period the Shutdown command drains and the thread exits;
        // past it the JoinHandle is dropped (detached) and the thread
        // exits on its own once the call completes.
        let joined = self
            .exited
            .as_ref()
            .is_none_or(|exited| exited.wait_exit(SHUTDOWN_GRACE));
        if joined {
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}

/// In-flight tool `on_update` callbacks (ADR-0015): an extension tool's
/// execute closure stashes `toolCallId → on_update` here for the duration of
/// its `toolExecute` dispatch, so `toolUpdate` host calls (guest → host,
/// re-entrant during the dispatch) reach the agent's partial-result sink.
/// The entry is removed when the dispatch returns — updates arriving after
/// that are dropped (upstream settle semantics, agent-loop.ts:1301-1302).
/// `std::sync::Mutex`: callbacks are invoked OUTSIDE the lock (cloned `Arc`).
pub(crate) type PendingToolUpdates = std::sync::Arc<
    std::sync::Mutex<
        std::collections::HashMap<
            String,
            std::sync::Arc<dyn Fn(rpi_agent::types::AgentToolResult) + Send + Sync>,
        >,
    >,
>;

/// State shared with the `rpi_host_call` import closure.
pub struct HostState {
    pub api: ExtensionApi,
    pub capabilities: HashSet<Capability>,
    pub async_handle: tokio::runtime::Handle,
    pub forward: DispatchTarget,
    /// Set around command-handler dispatches; `command.*` host calls are
    /// rejected outside one (upstream documents the deadlock hazard; here
    /// the check enforces the boundary).
    pub in_command: std::cell::Cell<bool>,
    /// Per-extension in-flight `on_update` sinks (ADR-0015). Lives on the
    /// guest Store (wasm) / plugin call context (native) — shared by the
    /// execute closures this extension registers.
    pub tool_updates: PendingToolUpdates,
}

/// The `rpi_host_call` response envelopes.
pub(crate) fn ok_response(value: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"ok": value})).unwrap_or_else(|_| b"{\"ok\":null}".to_vec())
}

pub(crate) fn error_response(kind: &str, message: String) -> Vec<u8> {
    serde_json::to_vec(&json!({"error": {"kind": kind, "message": message}})).unwrap_or_else(|_| {
        b"{\"error\":{\"kind\":\"internal\",\"message\":\"serialization\"}}".to_vec()
    })
}

/// Method dispatch for `rpi_host_call` (capability check first, then the
/// method table in `host_call`). Shared by the wasm import and the native
/// plugin trampoline.
pub(crate) fn handle_host_call(state: &mut HostState, request: &[u8]) -> Vec<u8> {
    let request: Value = match serde_json::from_slice(request) {
        Ok(value) => value,
        Err(error) => return error_response("invalidRequest", error.to_string()),
    };
    let method = request
        .get("call")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = request.get("args").cloned().unwrap_or(Value::Null);

    match host_call::required_capability(&method) {
        host_call::CapabilityRequirement::Free => {}
        host_call::CapabilityRequirement::Requires(capability) => {
            if !state.capabilities.contains(&capability) {
                return error_response(
                    "capabilityDenied",
                    format!("method \"{method}\" requires capability \"{capability:?}\""),
                );
            }
        }
        // Rejected here (not by `dispatch`) so the capability mapping stays
        // fail-closed: an unclassified method never reaches a dispatch arm.
        host_call::CapabilityRequirement::UnknownMethod => {
            return error_response("unknownMethod", format!("unknown host call: {method}"));
        }
    }

    match host_call::dispatch(state, &method, args) {
        Ok(value) => ok_response(value),
        Err((kind, message)) => error_response(kind, message),
    }
}

/// Instantiate a compiled module on its own thread and run
/// `rpi_extension_init`. The receipt JSON (`{"ok":...}` /
/// `{"error": {...}}`) decides the load outcome. Errors (missing exports,
/// traps, bad JSON) come back as `Err`, never panics.
pub async fn instantiate_and_init(
    module: &wasmtime::Module,
    api: ExtensionApi,
    capabilities: HashSet<Capability>,
) -> Result<WasmGuest, String> {
    let async_handle = tokio::runtime::Handle::current();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (tx, rx) = std::sync::mpsc::channel::<GuestCommand>();
    let exited = ExitSignal::new();
    let thread_exit = exited.clone();
    let forward = DispatchTarget::Wasm(WasmForward { tx: tx.clone() });
    let module = module.clone();

    let join = std::thread::spawn(move || {
        let mut store = wasmtime::Store::new(
            &engine(),
            HostState {
                api,
                capabilities,
                async_handle,
                forward,
                in_command: std::cell::Cell::new(false),
                tool_updates: PendingToolUpdates::default(),
            },
        );
        let mut linker = wasmtime::Linker::new(store.engine());
        let host_call = |mut caller: wasmtime::Caller<'_, HostState>, ptr: u32, len: u32| -> u64 {
            let bytes = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(memory)) => {
                    // Bounds-check the guest-controlled length against the
                    // linear memory BEFORE allocating: `len` may claim up to
                    // u32::MAX, and a multi-GiB zeroed host allocation would
                    // not be billed to the guest's fuel.
                    if len as usize > MAX_HOST_CALL_BYTES
                        || (ptr as usize).saturating_add(len as usize) > memory.data_size(&caller)
                    {
                        return pack_response(
                            &mut caller,
                            error_response(
                                "memoryAccess",
                                "host call payload out of bounds".to_owned(),
                            ),
                        );
                    }
                    let mut buffer = vec![0u8; len as usize];
                    match memory.read(&caller, ptr as usize, &mut buffer) {
                        Ok(()) => buffer,
                        Err(error) => {
                            return pack_response(
                                &mut caller,
                                error_response("memoryAccess", error.to_string()),
                            );
                        }
                    }
                }
                _ => {
                    return pack_response(
                        &mut caller,
                        error_response("missingExport", "guest has no memory export".to_owned()),
                    );
                }
            };
            let response = handle_host_call(caller.data_mut(), &bytes);
            pack_response(&mut caller, response)
        };
        if let Err(error) = linker.func_wrap("rpi", "rpi_host_call", host_call) {
            let _ = ready_tx.send(Err(format!("linker: {error}")));
            return;
        }
        let instance = match linker.instantiate(&mut store, &module) {
            Ok(instance) => instance,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("instantiate: {error}")));
                return;
            }
        };
        // Required exports (ABI v1).
        let init = match instance.get_typed_func::<(), u64>(&mut store, "rpi_extension_init") {
            Ok(func) => func,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("missing rpi_extension_init: {error}")));
                return;
            }
        };
        let dispatch = match instance.get_typed_func::<(u32, u32), u64>(&mut store, "rpi_dispatch")
        {
            Ok(func) => func,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("missing rpi_dispatch: {error}")));
                return;
            }
        };
        if let Err(error) = instance.get_typed_func::<u32, u32>(&mut store, "rpi_alloc") {
            let _ = ready_tx.send(Err(format!("missing rpi_alloc: {error}")));
            return;
        }
        if let Err(error) = instance.get_typed_func::<(u32, u32), ()>(&mut store, "rpi_dealloc") {
            let _ = ready_tx.send(Err(format!("missing rpi_dealloc: {error}")));
            return;
        }
        if instance.get_memory(&mut store, "memory").is_none() {
            let _ = ready_tx.send(Err("missing memory export".to_owned()));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        while let Ok(command) = rx.recv() {
            match command {
                GuestCommand::Init(respond) => {
                    let _ = store.set_fuel(CALL_FUEL);
                    let result = init
                        .call(&mut store, ())
                        .map_err(|e| e.to_string())
                        .and_then(|packed| read_packed(&mut store, &instance, packed));
                    let _ = respond.send(result);
                }
                GuestCommand::Dispatch {
                    message,
                    command_context,
                    respond,
                } => {
                    store.data().in_command.set(command_context);
                    let _ = store.set_fuel(CALL_FUEL);
                    let result = write_guest_bytes(&mut store, &instance, &message)
                        .and_then(|(ptr, len)| {
                            dispatch
                                .call(&mut store, (ptr, len))
                                .map_err(|e| e.to_string())
                        })
                        .and_then(|packed| read_packed(&mut store, &instance, packed));
                    store.data().in_command.set(false);
                    let _ = respond.send(result);
                }
                GuestCommand::Shutdown => break,
            }
        }
        thread_exit.signal_exit();
    });

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = join.join();
            return Err(error);
        }
        Err(_) => return Err("guest thread died during instantiation".to_owned()),
    }

    let guest = WasmGuest {
        forward: WasmForward { tx },
        join: Some(join),
        exited: Some(exited),
    };
    // `rpi_extension_init` — the receipt decides the load outcome.
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    guest
        .forward
        .tx
        .send(GuestCommand::Init(init_tx))
        .map_err(|_| "guest thread is gone".to_owned())?;
    let receipt = tokio::task::spawn_blocking(move || init_rx.recv())
        .await
        .map_err(|_| "guest thread dropped the init response".to_owned())?
        .map_err(|_| "guest thread dropped the init response".to_owned())??;
    if let Some(error) = receipt.get("error") {
        let kind = error.get("kind").and_then(Value::as_str).unwrap_or("call");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("guest init failed");
        return Err(format!("{kind}: {message}"));
    }
    Ok(guest)
}

/// Allocate + write guest memory via its `rpi_alloc`.
fn write_guest_bytes(
    store: &mut wasmtime::Store<HostState>,
    instance: &wasmtime::Instance,
    bytes: &[u8],
) -> Result<(u32, u32), String> {
    let alloc = instance
        .get_typed_func::<u32, u32>(&mut *store, "rpi_alloc")
        .map_err(|e| e.to_string())?;
    let _ = store.set_fuel(CALL_FUEL);
    let ptr = alloc
        .call(&mut *store, bytes.len() as u32)
        .map_err(|e| e.to_string())?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| "missing memory export".to_owned())?;
    memory
        .write(&mut *store, ptr as usize, bytes)
        .map_err(|e| e.to_string())?;
    Ok((ptr, bytes.len() as u32))
}

/// Read a packed `(ptr << 32) | len` guest string and free it via
/// `rpi_dealloc`.
fn read_packed(
    store: &mut wasmtime::Store<HostState>,
    instance: &wasmtime::Instance,
    packed: u64,
) -> Result<Value, String> {
    let ptr = (packed >> 32) as u32;
    let len = (packed & 0xffff_ffff) as u32;
    // `0` is the ABI's internal-error marker (allocation/write failure).
    if packed == 0 {
        return Err("guest returned 0 (internal error)".to_owned());
    }
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| "missing memory export".to_owned())?;
    // Guest-controlled return length: bounds-check against the linear
    // memory before allocating (see [`MAX_HOST_CALL_BYTES`]).
    if len as usize > MAX_HOST_CALL_BYTES
        || (ptr as usize).saturating_add(len as usize) > memory.data_size(&mut *store)
    {
        return Err("guest returned an out-of-bounds payload".to_owned());
    }
    let mut buffer = vec![0u8; len as usize];
    memory
        .read(&mut *store, ptr as usize, &mut buffer)
        .map_err(|e| e.to_string())?;
    if let Ok(dealloc) = instance.get_typed_func::<(u32, u32), ()>(&mut *store, "rpi_dealloc") {
        let _ = store.set_fuel(CALL_FUEL);
        let _ = dealloc.call(&mut *store, (ptr, len));
    }
    serde_json::from_slice(&buffer).map_err(|e| format!("guest returned invalid JSON: {e}"))
}

/// Pack a response into guest memory from inside the host import
/// (`Caller` context): allocate via the guest's `rpi_alloc` export.
fn pack_response(caller: &mut wasmtime::Caller<'_, HostState>, bytes: Vec<u8>) -> u64 {
    let len = bytes.len() as u32;
    let alloc = match caller.get_export("rpi_alloc") {
        Some(wasmtime::Extern::Func(func)) => func,
        _ => return 0,
    };
    let ptr = match alloc
        .typed::<u32, u32>(&mut *caller)
        .and_then(|f| f.call(&mut *caller, len))
    {
        Ok(ptr) => ptr,
        Err(_) => return 0,
    };
    let written = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(memory)) => {
            memory.write(&mut *caller, ptr as usize, &bytes).is_ok()
        }
        _ => false,
    };
    if !written {
        return 0;
    }
    ((ptr as u64) << 32) | (len as u64)
}
