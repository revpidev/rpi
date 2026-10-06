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

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use abi_stable::library::RootModule;
use abi_stable::sabi_types::VersionStrings;
use abi_stable::std_types::RVec;
use abi_stable::{StableAbi, package_version_strings};
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
    /// In-flight tool abort signals (see `PendingToolAborts`).
    tool_aborts: crate::wasm::PendingToolAborts,
    /// Live `on` host-call subscriptions (#8967, V15-09).
    subscriptions: crate::wasm::HostCallSubscriptions,
}

/// Live plugin contexts by cookie id.
///
/// The cookie is a value, not an address: the context is owned by this
/// registry for as long as the plugin is loaded, and every trampoline call
/// clones the `Arc` for the duration of the call. A plugin thread that calls
/// back after `/reload` dropped the plugin (for example the rpi-usage
/// refresh worker mid-fetch) gets a stale-context error envelope instead of
/// dereferencing freed memory (v0.1.6 review P2-12).
static CONTEXTS: OnceLock<Mutex<HashMap<usize, Arc<NativeCallContext>>>> = OnceLock::new();

fn contexts() -> &'static Mutex<HashMap<usize, Arc<NativeCallContext>>> {
    CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_cookie_id() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn lookup_context(cookie: PluginCookie) -> Option<Arc<NativeCallContext>> {
    let id = cookie as usize;
    if id == 0 {
        return None;
    }
    contexts()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&id)
        .cloned()
}

fn remove_context(id: usize) {
    contexts()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&id);
}

/// Response for a call on an unloaded plugin's context (the native ABI's
/// error envelope shape).
fn stale_context_response() -> RVec<u8> {
    let envelope = serde_json::json!({
        "error": {
            "kind": "stale",
            "message": "the extension was unloaded while this call was in flight",
        }
    });
    RVec::from(serde_json::to_vec(&envelope).unwrap_or_else(|_| b"null".to_vec()))
}

/// The host-side trampoline handed to plugins as `PluginHostCall`.
extern "C" fn host_call_trampoline(cookie: PluginCookie, request: RVec<u8>) -> RVec<u8> {
    // The registry owns the context; the clone keeps it alive for this call
    // even when the plugin is dropped concurrently (P2-12).
    let Some(context) = lookup_context(cookie) else {
        return stale_context_response();
    };
    let mut state = crate::wasm::HostState {
        api: context.api.clone(),
        capabilities: context.capabilities.clone(),
        async_handle: context.async_handle.clone(),
        forward: context.forward.clone(),
        in_command: std::cell::Cell::new(with_in_command(|cell| cell.get())),
        tool_updates: context.tool_updates.clone(),
        tool_aborts: context.tool_aborts.clone(),
        subscriptions: context.subscriptions.clone(),
        memory_limiter: crate::wasm::MemoryLimiter,
    };
    let response = crate::wasm::handle_host_call(&mut state, &request[..]);
    RVec::from(response)
}

/// A loaded native plugin (keeps the library mapped and its host-call
/// context registered).
pub struct NativePlugin {
    #[allow(dead_code)] // the field keeps the library mapped
    module: RpiNativeModule_Ref,
    /// Registry key; the context itself lives in [`CONTEXTS`].
    cookie: usize,
}

impl Drop for NativePlugin {
    fn drop(&mut self) {
        // Only this plugin's entry; in-flight trampolines keep their cloned
        // `Arc` and finish safely, and later calls answer `stale`.
        remove_context(self.cookie);
    }
}

/// Per-path `RpiNativeModule_Ref` load (no per-type memoization): open the
/// library, check the abi_stable layout, and materialize the module table
/// from THIS library's root-module loader symbol. The library is leaked by
/// `lib_header_from_raw_library` (documented) — the module refs it hands
/// out are `'static`.
fn load_native_module_at(path: &Path) -> Result<RpiNativeModule_Ref, String> {
    use abi_stable::library::{RawLibrary, lib_header_from_raw_library};
    let raw = RawLibrary::load_at(path).map_err(|e| format!("open: {e}"))?;
    // `lib_header_from_raw_library` does NOT leak — dropping `raw` would
    // dlclose the library and dangle every 'static ref handed out (the
    // in-tree `load_from` leaks it for exactly this reason; match it).
    let raw: &'static RawLibrary = Box::leak(Box::new(raw));
    // Safety: the library is leaked above (alive for the process lifetime);
    // the layout check below makes the module-table read layout-compatible.
    let header =
        unsafe { lib_header_from_raw_library(raw) }.map_err(|e| format!("root module: {e}"))?;
    header
        .ensure_layout::<RpiNativeModule_Ref>()
        .map_err(|e| format!("layout: {e}"))?;
    // Safety: layout (and abi version, checked inside) verified above.
    unsafe { header.init_root_module_with_unchecked_layout::<RpiNativeModule_Ref>() }
        .map_err(|e| format!("init: {e}"))
}

/// Load a native plugin (`loadExtension` for a dynamic library): load the
/// module, run `rpi_extension_init`, keep the handles on the extension.
pub async fn load_native_plugin(
    path: &Path,
    api: ExtensionApi,
    capabilities: HashSet<Capability>,
) -> Result<(), String> {
    // Per-path module load. `RpiNativeModule_Ref::load_from_file` CANNOT be
    // used here: `RootModule::load_from` memoizes in a per-TYPE global
    // (`root_module_statics().root_mod.try_init`) — "once the root module is
    // loaded, this will return the already loaded root module" — so the
    // second plugin path would silently get the FIRST plugin's module table
    // and re-run its init (found loading all three plugins together: every
    // extension registered the first .so's tools). The raw-library face
    // below loads each path independently; `lib_header_from_raw_library`
    // leaks the library, matching the `'static` module refs `NativePlugin`
    // already keeps for the process lifetime.
    let module = load_native_module_at(path)
        .map_err(|e| format!("load dynamic library {}: {e}", path.display()))?;
    let dispatch_fn = module.rpi_dispatch();
    let init_fn = module.rpi_extension_init();

    let cookie_id = next_cookie_id();
    let cookie_ptr = cookie_id as PluginCookie;
    let context = Arc::new(NativeCallContext {
        api: api.clone(),
        capabilities,
        async_handle: tokio::runtime::Handle::current(),
        forward: DispatchTarget::Native(NativeForward {
            dispatch_fn,
            cookie: cookie_id,
        }),
        tool_updates: crate::wasm::PendingToolUpdates::default(),
        tool_aborts: crate::wasm::PendingToolAborts::default(),
        subscriptions: crate::wasm::HostCallSubscriptions::default(),
    });
    contexts()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(cookie_id, context.clone());

    let receipt_bytes = init_fn(
        RpiHostCalls {
            call: host_call_trampoline,
        },
        cookie_ptr,
    );
    let receipt: Value = match serde_json::from_slice(&receipt_bytes[..]) {
        Ok(receipt) => receipt,
        Err(error) => {
            // A failed load must not leave a registered context behind.
            remove_context(cookie_id);
            return Err(format!("plugin init returned invalid JSON: {error}"));
        }
    };
    if let Some(error) = receipt.get("error") {
        let kind = error.get("kind").and_then(Value::as_str).unwrap_or("call");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("plugin init failed");
        remove_context(cookie_id);
        return Err(format!("{kind}: {message}"));
    }

    api.extension().set_native_plugin(NativePlugin {
        module,
        cookie: cookie_id,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ExtensionApi, ExtensionRuntime, LoadedExtension};

    extern "C" fn stub_dispatch(_cookie: PluginCookie, _message: RVec<u8>) -> RVec<u8> {
        RVec::from(b"null".to_vec())
    }

    fn test_context(runtime: &tokio::runtime::Runtime) -> Arc<NativeCallContext> {
        Arc::new(NativeCallContext {
            api: ExtensionApi::for_extension(
                Arc::new(LoadedExtension::new(
                    "<inline:native-registry>",
                    "<inline:native-registry>",
                )),
                ExtensionRuntime::new(),
                "/test-cwd",
            ),
            capabilities: HashSet::new(),
            async_handle: runtime.handle().clone(),
            forward: DispatchTarget::Native(NativeForward {
                dispatch_fn: stub_dispatch,
                cookie: 0,
            }),
            tool_updates: crate::wasm::PendingToolUpdates::default(),
            tool_aborts: crate::wasm::PendingToolAborts::default(),
            subscriptions: crate::wasm::HostCallSubscriptions::default(),
        })
    }

    /// v0.1.6 review P2-12: the trampoline resolves the context through the
    /// registry, so a plugin thread that calls back after unload gets a
    /// stale-context envelope instead of dereferencing freed memory.
    #[test]
    fn removed_context_answers_stale_instead_of_dangling() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let id = next_cookie_id();
        contexts()
            .lock()
            .unwrap()
            .insert(id, test_context(&runtime));
        assert!(lookup_context(id as PluginCookie).is_some());
        remove_context(id);
        assert!(lookup_context(id as PluginCookie).is_none());

        let response = host_call_trampoline(
            id as PluginCookie,
            RVec::from(br#"{"call":"getFlag","args":{}}"#.to_vec()),
        );
        let response: Value = serde_json::from_slice(&response[..]).expect("stale JSON envelope");
        assert_eq!(response["error"]["kind"], "stale");
        assert!(lookup_context(std::ptr::null()).is_none());
    }
}
