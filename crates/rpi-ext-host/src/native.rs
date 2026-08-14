//! Native (L0) dynamic-library plugin support — abi_stable (design §14
//! pinned; T15 W7).
//!
//! The plugin ABI mirrors the wasm ABI v1 message formats exactly
//! (docs/extension-abi.md §2): the same JSON method table and capability
//! checks apply (`rpi_host_call` → [`PluginHostCall`], dispatch → the
//! exported `rpi_dispatch` function). Differences from the wasm guest:
//! in-process (no thread/Store), plugins get full host OS access (the
//! capability system gates the extension API surface only — native code is
//! inherently unsandboxed; this is the documented L0 trust model).

// abi_stable's `#[sabi(kind(Prefix(...)))]` generates a `<Name>_Ref` type.
#![allow(non_camel_case_types)]

use std::collections::HashSet;
use std::path::Path;

use abi_stable::library::RootModule;
use abi_stable::sabi_types::VersionStrings;
use abi_stable::std_types::RVec;
use abi_stable::{package_version_strings, StableAbi};
use serde_json::Value;

use crate::api::ExtensionApi;
use crate::wasm::{Capability, DispatchTarget, NativeForward};

/// Opaque context pointer passed to plugins (addresses the plugin's
/// [`NativeCallContext`]); `*const c_void` because abi_stable lays out raw
/// pointers but not `usize`.
pub type PluginCookie = *const std::ffi::c_void;

/// The host-call handle bundle handed to `rpi_extension_init` BY VALUE —
/// abi_stable cannot lay out fn-pointers as fn params, so the handle rides
/// a `repr(C)` struct. Buffers are owned (`RVec`) both ways (borrowed
/// slices would put lifetimes in the fn-pointer type).
#[repr(C)]
#[derive(StableAbi)]
pub struct RpiHostCalls {
    /// `(cookie, request JSON) -> response JSON` — the `rpi_host_call`
    /// equivalent (docs/extension-abi.md §2.1).
    pub call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
}

/// The plugin root module: export this from the cdylib with
/// `#[export_root_module]` (see `crates/rpi-test-native-plugin`).
#[repr(C)]
#[derive(StableAbi)]
#[sabi(kind(Prefix(prefix_ref = RpiNativeModule_Ref)))]
#[sabi(missing_field(panic))]
pub struct RpiNativeModule {
    /// Load entry: registers via the host-call handle, returns the init
    /// receipt JSON (`{"ok": true}` / `{"error": {...}}`).
    pub rpi_extension_init: extern "C" fn(RpiHostCalls, PluginCookie) -> RVec<u8>,
    /// Dispatch entry (event/toolExecute/command/shortcut/render/bus).
    #[sabi(last_prefix_field)]
    pub rpi_dispatch: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
}

impl RootModule for RpiNativeModule_Ref {
    abi_stable::declare_root_module_statics! {RpiNativeModule_Ref}

    const BASE_NAME: &'static str = "rpi_native_extension";
    const NAME: &'static str = "rpi_native_extension";
    const VERSION_STRINGS: VersionStrings = package_version_strings!();
}

/// Read/replace the calling thread's command-context flag (used by
/// [`crate::wasm::NativeForward::dispatch`]).
///
/// Per-thread by design (T15 W7): the trampoline must answer "am I inside a
/// command-handler dispatch?" for THIS call site. A shared atomic races
/// across concurrent dispatches (agent emit vs TUI render) and its
/// store-back can clobber the other thread's value; a thread-local mirrors
/// the per-call context exactly.
pub(crate) fn with_in_command<R>(f: impl FnOnce(&std::cell::Cell<bool>) -> R) -> R {
    thread_local! {
        static IN_COMMAND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    IN_COMMAND.with(f)
}

/// Per-plugin host-call context (the cookie's pointee).
struct NativeCallContext {
    api: ExtensionApi,
    capabilities: HashSet<Capability>,
    async_handle: tokio::runtime::Handle,
    forward: DispatchTarget,
    /// In-flight `on_update` sinks (ADR-0015); lives as long as the plugin,
    /// shared by every re-entrant host call.
    tool_updates: crate::wasm::PendingToolUpdates,
}

/// The host-side trampoline handed to plugins as `PluginHostCall`.
extern "C" fn host_call_trampoline(cookie: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // The cookie lives in the NativePlugin holder on the LoadedExtension
    // (outlives every call); align/validity is guaranteed by construction.
    let context = unsafe { &*(cookie as *const NativeCallContext) };
    let mut state = crate::wasm::HostState {
        api: context.api.clone(),
        capabilities: context.capabilities.clone(),
        async_handle: context.async_handle.clone(),
        forward: context.forward.clone(),
        in_command: std::cell::Cell::new(with_in_command(|cell| cell.get())),
        tool_updates: context.tool_updates.clone(),
    };
    let response = crate::wasm::handle_host_call(&mut state, &request[..]);
    RVec::from(response)
}

/// A loaded native plugin (keeps the library and call context alive).
pub struct NativePlugin {
    #[allow(dead_code)] // the field keeps the library mapped
    module: RpiNativeModule_Ref,
    #[allow(dead_code)] // owns the cookie pointee
    context: Box<NativeCallContext>,
}

/// Load a native plugin (`loadExtension` for a dynamic library): load the
/// module, run `rpi_extension_init`, keep the handles on the extension.
pub async fn load_native_plugin(
    path: &Path,
    api: ExtensionApi,
    capabilities: HashSet<Capability>,
) -> Result<(), String> {
    let module = RpiNativeModule_Ref::load_from_file(path)
        .map_err(|e| format!("load dynamic library {}: {e}", path.display()))?;
    let dispatch_fn = module.rpi_dispatch();
    let init_fn = module.rpi_extension_init();

    let cookie_box = Box::new(NativeCallContext {
        api: api.clone(),
        capabilities,
        async_handle: tokio::runtime::Handle::current(),
        // Placeholder replaced below (the forward needs the cookie).
        forward: DispatchTarget::Native(NativeForward {
            dispatch_fn,
            cookie: 0,
        }),
        tool_updates: crate::wasm::PendingToolUpdates::default(),
    });
    let cookie_ptr = &*cookie_box as *const NativeCallContext as PluginCookie;
    let cookie = cookie_ptr as usize;
    let mut context = cookie_box;
    context.forward = DispatchTarget::Native(NativeForward {
        dispatch_fn,
        cookie,
    });

    let receipt_bytes = init_fn(
        RpiHostCalls {
            call: host_call_trampoline,
        },
        cookie_ptr,
    );
    let receipt: Value = serde_json::from_slice(&receipt_bytes[..])
        .map_err(|e| format!("plugin init returned invalid JSON: {e}"))?;
    if let Some(error) = receipt.get("error") {
        let kind = error.get("kind").and_then(Value::as_str).unwrap_or("call");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("plugin init failed");
        return Err(format!("{kind}: {message}"));
    }

    api.extension()
        .set_native_plugin(NativePlugin { module, context });
    Ok(())
}
