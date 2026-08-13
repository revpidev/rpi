//! `NativeExtensionHost` (L0) — composes loader + runner core into the
//! surface the `rpi` crate drives @ pi 0.82.1 (2efa728).
//!
//! Upstream counterpart: the `ExtensionRunner` (extensions/runner.ts) plus
//! the loading half of `DefaultResourceLoader`
//! (resource-loader.ts:494-560). The rpi-side adapter
//! (`rpi::core::extension_host_adapter`) maps this host onto the session's
//! `ExtensionRunner` seam.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::api::{EventBus, ExtensionRuntime, HostActions, InsertionMap, UiBridge};
use crate::error::ExtError;
use crate::loader::{DiscoverConfig, ExtensionLoader, PreTrustRecord};
use crate::runner::{ExtensionErrorListener, ExtensionRunnerCore};
use crate::types::{
    ExtensionError, ExtensionFlag, ExtensionMode, ExtensionShortcut, HostDiagnostic,
    RegisteredTool, ResolvedCommand,
};

/// The last full load inputs, replayed by [`NativeExtensionHost::reload`]
/// (T15 W5).
#[derive(Clone)]
struct LoadSpec {
    agent_dir: PathBuf,
    cli_paths: Vec<String>,
    package_paths: Vec<String>,
    inline: Vec<crate::loader::InlineExtension>,
    include_project_local: bool,
    no_extensions: bool,
}

/// The L0 extension host: native (boxed-closure) extensions over one shared
/// runtime, with the wasm (L1) backend landing in W6 behind the same
/// capability surface.
///
/// Interior-mutable (`RwLock` core/runtime): the host is shared via `Arc`
/// through the session's runner slot, and `/reload` swaps the extension set
/// + runtime in place so the adapter stays valid (T15 W5).
pub struct NativeExtensionHost {
    runtime: RwLock<ExtensionRuntime>,
    loader: RwLock<ExtensionLoader>,
    core: RwLock<Arc<ExtensionRunnerCore>>,
    cwd: String,
    pre_trust: RwLock<Option<PreTrustRecord>>,
    last_load: RwLock<Option<LoadSpec>>,
    last_ui: RwLock<Option<(Arc<dyn UiBridge>, ExtensionMode)>>,
}

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

impl NativeExtensionHost {
    /// Empty host (no extensions loaded): every `has_handlers` is false,
    /// emits are no-ops, registries are empty — mirroring upstream with zero
    /// extensions.
    pub fn new(cwd: &str) -> Self {
        let runtime = ExtensionRuntime::new();
        let loader = ExtensionLoader::new(runtime.clone());
        let core = ExtensionRunnerCore::new(Vec::new(), runtime.clone(), cwd.to_owned());
        NativeExtensionHost {
            runtime: RwLock::new(runtime),
            loader: RwLock::new(loader),
            core: RwLock::new(Arc::new(core)),
            cwd: cwd.to_owned(),
            pre_trust: RwLock::new(None),
            last_load: RwLock::new(None),
            last_ui: RwLock::new(None),
        }
    }

    pub fn runtime(&self) -> ExtensionRuntime {
        read(&self.runtime).clone()
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Shared handle to the runner core (derefs to `&ExtensionRunnerCore`;
    /// cloned out so no lock is held across `.await`).
    pub fn core(&self) -> Arc<ExtensionRunnerCore> {
        read(&self.core).clone()
    }

    pub fn event_bus(&self) -> EventBus {
        self.runtime().event_bus()
    }

    /// Replace the loaded extension set (load methods + `/reload`).
    fn install(&self, extensions: Vec<Arc<crate::api::LoadedExtension>>) {
        *write(&self.core) = Arc::new(ExtensionRunnerCore::new(
            extensions,
            self.runtime(),
            self.cwd.clone(),
        ));
    }

    /// Load inline (native factory) extensions — the W1 entry point for
    /// built-in extensions. Appends to the loaded set (upstream
    /// `loadExtensions` results accumulate into the runner's extension
    /// list). Returns load errors; failed factories are isolated
    /// (loader.ts:454-480).
    pub async fn load_inline(
        &self,
        inline: &[crate::loader::InlineExtension],
    ) -> Vec<crate::types::ExtensionLoadError> {
        let loader = read(&self.loader).clone();
        let result = loader.load_inline(inline, &PathBuf::from(&self.cwd)).await;
        let mut extensions = self.core().extensions().to_vec();
        extensions.extend(result.extensions);
        self.install(extensions);
        // Record for `/reload` replay (upstream keeps inline factories in
        // `resourceLoader.extensionFactories`).
        {
            let mut last_load = write(&self.last_load);
            let spec = last_load.get_or_insert_with(|| LoadSpec {
                agent_dir: PathBuf::new(),
                cli_paths: Vec::new(),
                package_paths: Vec::new(),
                inline: Vec::new(),
                include_project_local: false,
                no_extensions: false,
            });
            spec.inline.extend(inline.iter().cloned());
        }
        result.errors
    }

    /// Load explicit `.wasm` paths (loader error isolation; T15 W6).
    pub async fn load_paths(&self, paths: &[PathBuf]) -> Vec<crate::types::ExtensionLoadError> {
        let loader = read(&self.loader).clone();
        let result = loader.load_paths(paths, &PathBuf::from(&self.cwd)).await;
        let mut extensions = self.core().extensions().to_vec();
        extensions.extend(result.extensions);
        self.install(extensions);
        result.errors
    }

    /// Full discovery + load (`discoverAndLoadExtensions`,
    /// loader.ts:673-721; order per resource-loader.ts:494-514).
    pub async fn discover_and_load(
        &self,
        agent_dir: PathBuf,
        cli_paths: Vec<String>,
        package_paths: Vec<String>,
        inline: Vec<crate::loader::InlineExtension>,
    ) -> Vec<crate::types::ExtensionLoadError> {
        self.load_startup_final(agent_dir, cli_paths, package_paths, inline, true, false)
            .await
    }

    /// Pre-trust bootstrap load (`loadProjectTrustExtensions`,
    /// resource-loader.ts:327-335): user/global + CLI `-e` + inline
    /// factories; project-local extensions stay out while trust is
    /// unresolved, and `--no-extensions` narrows the pass to CLI `-e` +
    /// inline exactly like the final pass (resource-loader.ts:500-504). The
    /// loaded set is recorded for
    /// [`NativeExtensionHost::load_startup_final`] reuse.
    pub async fn load_startup_pre_trust(
        &self,
        agent_dir: PathBuf,
        cli_paths: Vec<String>,
        inline: Vec<crate::loader::InlineExtension>,
        no_extensions: bool,
    ) -> Vec<crate::types::ExtensionLoadError> {
        let config = DiscoverConfig {
            cwd: PathBuf::from(&self.cwd),
            agent_dir,
            cli_paths,
            package_paths: Vec::new(),
            inline,
            include_project_local: false,
            no_extensions,
        };
        let loader = read(&self.loader).clone();
        let result = loader.discover_and_load(&config).await;
        let errors = result.errors.clone();
        *write(&self.pre_trust) = Some(PreTrustRecord {
            extensions: result.extensions.clone(),
            errors: result.errors,
        });
        self.install(result.extensions);
        errors
    }

    /// Final startup load (`loadFinalExtensionSet`,
    /// resource-loader.ts:520-571). With a recorded pre-trust pass its
    /// extensions are reused (inline factories are not re-run) and its
    /// errors carried over; without one this is a fresh full load.
    /// `include_project_local` gates `.rpi/extensions` discovery on the
    /// resolved trust state. The returned errors include the extension
    /// conflict diagnostics (`addExtensionConflictDiagnostics`,
    /// resource-loader.ts:573-581).
    ///
    /// `inline` factories are recorded in the reload spec even when a
    /// pre-trust record already holds them (they are NOT re-run in the
    /// reuse pass, but `/reload` replays the spec and must re-run them,
    /// resource-loader.ts:360-363 `loadExtensionFactories`).
    #[allow(clippy::too_many_arguments)]
    pub async fn load_startup_final(
        &self,
        agent_dir: PathBuf,
        cli_paths: Vec<String>,
        package_paths: Vec<String>,
        inline: Vec<crate::loader::InlineExtension>,
        include_project_local: bool,
        no_extensions: bool,
    ) -> Vec<crate::types::ExtensionLoadError> {
        let config = DiscoverConfig {
            cwd: PathBuf::from(&self.cwd),
            agent_dir,
            cli_paths,
            package_paths,
            inline,
            include_project_local,
            no_extensions,
        };
        *write(&self.last_load) = Some(LoadSpec {
            agent_dir: config.agent_dir.clone(),
            cli_paths: config.cli_paths.clone(),
            package_paths: config.package_paths.clone(),
            inline: config.inline.clone(),
            include_project_local: config.include_project_local,
            no_extensions: config.no_extensions,
        });
        let record = write(&self.pre_trust).take();
        let loader = read(&self.loader).clone();
        let result = match record {
            Some(record) => loader.discover_and_load_reuse(&config, &record).await,
            None => loader.discover_and_load(&config).await,
        };
        let mut errors = result.errors;
        self.install(result.extensions);
        // `addExtensionConflictDiagnostics` (resource-loader.ts:573-581):
        // conflicts ride the error list; every extension stays loaded.
        for conflict in self.core().detect_extension_conflicts() {
            errors.push(crate::types::ExtensionLoadError {
                path: conflict.path.unwrap_or_default(),
                error: conflict.message,
            });
        }
        errors
    }

    /// Bind host action implementations (`bindCore` action half,
    /// runner.ts:311-408). Flushes queued provider registrations.
    pub async fn bind_actions(&self, actions: Arc<dyn HostActions>) {
        self.core().bind_actions(actions).await;
    }

    /// `/reload` (agent-session.ts:2600-2628 host half): stale the old
    /// runtime, preserve flag values (resource-loader reload keeps them),
    /// bump the factory-cache generation (loader.ts:151-155), re-run the
    /// last load against a fresh runtime, and re-apply the UI bridge. The
    /// session-level caller re-binds actions and emits
    /// `session_start`/`resources_discover` with reason `"reload"`.
    pub async fn reload(&self) -> Vec<crate::types::ExtensionLoadError> {
        let spec = read(&self.last_load).clone();
        let previous_flags = self.runtime().flag_values();

        // Old runtime goes stale (loader.ts:201-205).
        self.runtime().invalidate(None);
        // `clearExtensionCache` (loader.ts:151-155).
        read(&self.loader).cache().clear();

        let new_runtime = ExtensionRuntime::new();
        for (name, value) in previous_flags {
            new_runtime.set_flag_value(&name, value);
        }
        if let Some((bridge, mode)) = read(&self.last_ui).clone() {
            new_runtime.set_ui_bridge(Some(bridge), mode);
        }
        *write(&self.runtime) = new_runtime.clone();
        write(&self.loader).set_runtime(new_runtime);

        let Some(spec) = spec else {
            self.install(Vec::new());
            return Vec::new();
        };
        let config = DiscoverConfig {
            cwd: PathBuf::from(&self.cwd),
            agent_dir: spec.agent_dir,
            cli_paths: spec.cli_paths,
            package_paths: spec.package_paths,
            inline: spec.inline,
            include_project_local: spec.include_project_local,
            no_extensions: spec.no_extensions,
        };
        let loader = read(&self.loader).clone();
        let result = loader.discover_and_load(&config).await;
        let mut errors = result.errors;
        self.install(result.extensions);
        for conflict in self.core().detect_extension_conflicts() {
            errors.push(crate::types::ExtensionLoadError {
                path: conflict.path.unwrap_or_default(),
                error: conflict.message,
            });
        }
        errors
    }

    /// `createCommandContext` — command-handler context
    /// (runner.ts:740-777).
    pub fn create_command_context(&self) -> crate::api::ExtensionCommandContext {
        self.core().create_command_context()
    }

    /// `setUIContext` (runner.ts:429-432).
    pub fn set_ui(&self, ui: Option<Arc<dyn UiBridge>>, mode: ExtensionMode) {
        *write(&self.last_ui) = ui.clone().map(|bridge| (bridge, mode));
        self.runtime().set_ui_bridge(ui, mode);
    }

    /// Drop the UI bridge without touching the mode — dispose paths use
    /// this to break host → bridge → mode-resource reference cycles (the
    /// RPC bridge holds the stdout sender; the interactive bridge holds the
    /// UI weakly).
    pub fn clear_ui(&self) {
        *write(&self.last_ui) = None;
        self.runtime().clear_ui_bridge();
    }

    // ========================================================================
    // Emit surface (called by the rpi-side adapter / emit sites)
    // ========================================================================

    pub async fn emit(&self, event_type: &str, payload: Value) -> Option<Value> {
        self.core().emit(event_type, payload).await
    }

    pub async fn emit_message_end(&self, payload: Value) -> Option<Value> {
        self.core().emit_message_end(payload).await
    }

    pub async fn emit_tool_result(&self, payload: Value) -> Option<Value> {
        self.core().emit_tool_result(payload).await
    }

    pub async fn emit_tool_call(&self, payload: Value) -> Result<Option<Value>, ExtensionError> {
        self.core().emit_tool_call(payload).await
    }

    pub async fn emit_user_bash(&self, payload: Value) -> Option<Value> {
        self.core().emit_user_bash(payload).await
    }

    pub async fn emit_context(&self, messages: Value) -> Value {
        self.core().emit_context(messages).await
    }

    pub async fn emit_before_provider_request(&self, payload: Value) -> Value {
        self.core().emit_before_provider_request(payload).await
    }

    pub async fn emit_before_provider_headers(&self, headers: Value) -> Value {
        self.core().emit_before_provider_headers(headers).await
    }

    pub async fn emit_before_agent_start(&self, payload: Value) -> Option<Value> {
        self.core().emit_before_agent_start(payload).await
    }

    pub async fn emit_resources_discover(&self, payload: Value) -> Value {
        self.core().emit_resources_discover(payload).await
    }

    pub async fn emit_input(&self, payload: Value) -> Value {
        self.core().emit_input(payload).await
    }

    pub async fn emit_project_trust(&self, payload: Value) -> (Option<Value>, Vec<ExtensionError>) {
        self.core().emit_project_trust(payload).await
    }

    // ========================================================================
    // Registry queries (conflict-resolved)
    // ========================================================================

    pub fn has_handlers(&self, event_type: &str) -> bool {
        self.core().has_handlers(event_type)
    }

    pub fn get_all_registered_tools(&self) -> Vec<RegisteredTool> {
        self.core().get_all_registered_tools()
    }

    pub fn get_tool_definition(&self, tool_name: &str) -> Option<crate::types::ToolDefinition> {
        self.core().get_tool_definition(tool_name)
    }

    pub fn get_flags(&self) -> InsertionMap<ExtensionFlag> {
        self.core().get_flags()
    }

    pub fn get_shortcuts(
        &self,
        builtin_keybindings: &[(String, Vec<String>)],
    ) -> InsertionMap<ExtensionShortcut> {
        self.core().get_shortcuts(builtin_keybindings)
    }

    pub fn get_shortcut_diagnostics(&self) -> Vec<HostDiagnostic> {
        self.core().get_shortcut_diagnostics()
    }

    pub fn get_registered_commands(&self) -> Vec<ResolvedCommand> {
        self.core().get_registered_commands()
    }

    pub fn get_command(&self, name: &str) -> Option<ResolvedCommand> {
        self.core().get_command(name)
    }

    pub fn get_command_diagnostics(&self) -> Vec<HostDiagnostic> {
        self.core().get_command_diagnostics()
    }

    pub fn detect_extension_conflicts(&self) -> Vec<HostDiagnostic> {
        self.core().detect_extension_conflicts()
    }

    pub fn get_message_renderer(&self, custom_type: &str) -> Option<crate::types::MessageRenderFn> {
        self.core().get_message_renderer(custom_type)
    }

    pub fn get_markdown_transformers(&self) -> Vec<crate::types::MarkdownTransformerFn> {
        self.core().get_markdown_transformers()
    }

    pub fn get_entry_renderer(&self, custom_type: &str) -> Option<crate::types::EntryRenderFn> {
        self.core().get_entry_renderer(custom_type)
    }

    pub fn get_extension_paths(&self) -> Vec<String> {
        self.core().get_extension_paths()
    }

    // ========================================================================
    // Errors + stale lifecycle
    // ========================================================================

    /// `onError` (runner.ts:554-557).
    pub fn on_error(&self, listener: ExtensionErrorListener) -> crate::api::Unsubscribe {
        self.core().on_error(listener)
    }

    /// `emitError` (runner.ts:559-563).
    pub fn emit_error(&self, error: ExtensionError) {
        self.core().emit_error(error);
    }

    /// `invalidate` (runner.ts:539-546) — marks captured contexts stale.
    pub fn invalidate(&self, message: Option<String>) {
        self.core().invalidate(message);
    }

    /// `assertActive` (runner.ts:548-552).
    pub fn assert_active(&self) -> Result<(), ExtError> {
        self.core().assert_active()
    }
}
