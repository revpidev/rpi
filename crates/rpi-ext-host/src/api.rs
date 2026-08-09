//! Extension API (the `pi` object) + host-injected bridges @ pi 0.82.1
//! (2efa728).
//!
//! Ports:
//! - `createExtensionAPI` (extensions/loader.ts:230-393) — registration
//!   methods write into the extension object, action methods delegate to the
//!   shared runtime
//! - `createExtensionRuntime` throwing-stub semantics (loader.ts:166-223):
//!   action methods fail with [`ExtError::Unbound`] until the host binds
//!   [`HostActions`]
//! - `EventBus` (core/event-bus.ts)
//! - `Extension` (extensions/types.ts:1662-1675) as [`LoadedExtension`]
//!
//! [`HostActions`] / [`UiBridge`] are defined here (rpi-ext-host) and
//! implemented by the `rpi` crate per mode (W3/W4); the W1 host runs
//! unbound, in which case every action/UI method returns
//! [`ExtError::Unbound`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ExtError;
use crate::types::{
    ArgumentCompletionsFn, CommandHandlerFn, ComponentTree, EntryRenderFn, ExtSourceInfo,
    ExtensionFlag, ExtensionMode, ExtensionShortcut, FlagType, FlagValue, MessageRenderFn,
    RegisteredCommand, RegisteredTool, ShortcutHandlerFn, ToolDefinition,
};

/// Boxed future used throughout the host boundary.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Unsubscribe closure (`() => void` upstream).
pub type Unsubscribe = Box<dyn FnOnce() + Send>;

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

// ============================================================================
// Event bus (core/event-bus.ts)
// ============================================================================

type BusHandler = Arc<dyn Fn(Value) + Send + Sync>;

/// `EventBus` (event-bus.ts:3-14) — shared per extension runtime, exposed to
/// extensions as `pi.events`. Synchronous dispatch; handler errors are
/// logged, not propagated (upstream `safeHandler`, event-bus.ts:15-18).
/// Upstream handlers may return promises; ours are sync — a handler that
/// needs async work must spawn it (documented deviation: the bus never
/// awaits).
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

#[derive(Default)]
struct EventBusInner {
    handlers: RwLock<HashMap<String, Vec<(u64, BusHandler)>>>,
    next_id: AtomicU64,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// `emit(channel, data)`.
    pub fn emit(&self, channel: &str, data: Value) {
        let snapshot: Vec<BusHandler> = match read(&self.inner.handlers).get(channel) {
            Some(handlers) => handlers.iter().map(|(_, h)| h.clone()).collect(),
            None => return,
        };
        for handler in snapshot {
            // std::panic::catch_unwind would be overkill; a panicking bus
            // handler is a bug (coding-standards §5.3), unlike upstream's
            // per-handler try/catch which guards user JS.
            handler(data.clone());
        }
    }

    /// `on(channel, handler)` — returns an unsubscribe closure.
    pub fn on(&self, channel: &str, handler: BusHandler) -> Unsubscribe {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        write(&self.inner.handlers)
            .entry(channel.to_owned())
            .or_default()
            .push((id, handler));
        let inner = self.inner.clone();
        let channel = channel.to_owned();
        Box::new(move || {
            if let Some(handlers) = write(&inner.handlers).get_mut(&channel) {
                handlers.retain(|(hid, _)| *hid != id);
            }
        })
    }

    /// `clear()` (EventBusController).
    pub fn clear(&self) {
        write(&self.inner.handlers).clear();
    }
}

// ============================================================================
// Insertion-ordered string map
// ============================================================================

/// JS `Map<string, V>` semantics needed by the conflict rules: iteration in
/// first-insertion order, `set` on an existing key replaces the value but
/// keeps its position (runner.ts:446-629 iterates `ext.tools.values()` etc.).
#[derive(Debug, Clone)]
pub struct InsertionMap<V> {
    entries: Vec<(String, V)>,
    index: HashMap<String, usize>,
}

impl<V> Default for InsertionMap<V> {
    fn default() -> Self {
        InsertionMap {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<V> InsertionMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Map.set` — replace in place when the key exists (position kept).
    pub fn set(&mut self, key: String, value: V) {
        if let Some(&i) = self.index.get(&key) {
            self.entries[i].1 = value;
        } else {
            self.index.insert(key.clone(), self.entries.len());
            self.entries.push((key, value));
        }
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|&i| &self.entries[i].1)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `Map.values()` — insertion order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|(_, v)| v)
    }

    /// `Map.entries()` — insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }
}

// ============================================================================
// Host actions (ExtensionActions, types.ts:1591-1610 + provider/exec surface)
// ============================================================================

/// `deliverAs` for `sendMessage` (`"steer" | "followUp" | "nextTurn"`,
/// types.ts:1282).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliverAs {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
    #[serde(rename = "nextTurn")]
    NextTurn,
}

/// `sendMessage` options (types.ts:1280-1283).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_turn: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliver_as: Option<DeliverAs>,
}

/// `sendUserMessage` options (types.ts:1289-1292). No `nextTurn`: upstream
/// rejects it for streaming delivery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendUserMessageOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliver_as: Option<DeliverAs>,
}

/// `ExecOptions` (exec.ts:11-18). `signal` is not serializable; cancellation
/// support lands with the W3 binding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `ExecResult` (exec.ts:23-28).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub killed: bool,
}

/// Host-side implementations behind the `pi.*` action methods
/// (`ExtensionActions`, types.ts:1591-1610, plus `exec` and provider
/// registration from the same interface). Complex payloads cross as
/// camelCase JSON ([`Value`]) matching the upstream shapes:
/// `CustomMessage` pick, `ToolInfo`, `SlashCommandInfo`, `ProviderConfig`,
/// `Model`.
///
/// Implemented by the `rpi` crate in W3 (`bindCore`, runner.ts:311-408).
#[async_trait::async_trait]
pub trait HostActions: Send + Sync {
    /// `sendMessage` (types.ts:1280-1283). `message` is the
    /// `Pick<CustomMessage, "customType"|"content"|"display"|"details">` JSON.
    fn send_message(&self, message: Value, options: Option<SendMessageOptions>);

    /// `sendUserMessage` (types.ts:1289-1292). `content` is a string or
    /// `(TextContent | ImageContent)[]` JSON.
    fn send_user_message(&self, content: Value, options: Option<SendUserMessageOptions>);

    /// `appendEntry` (types.ts:1295).
    fn append_entry(&self, custom_type: &str, data: Option<Value>);

    /// `setSessionName` / `getSessionName` (types.ts:1302-1305).
    fn set_session_name(&self, name: &str);
    fn get_session_name(&self) -> Option<String>;

    /// `setLabel` (types.ts:1308).
    fn set_label(&self, entry_id: &str, label: Option<&str>);

    /// `exec` (types.ts:1311 → exec.ts `execCommand`).
    async fn exec(
        &self,
        command: &str,
        args: &[String],
        options: Option<ExecOptions>,
    ) -> Result<ExecResult, ExtError>;

    /// `getActiveTools` (types.ts:1314).
    fn get_active_tools(&self) -> Vec<String>;

    /// `getAllTools` (types.ts:1317) — `ToolInfo[]` JSON (types.ts:1546-1548).
    fn get_all_tools(&self) -> Vec<Value>;

    /// `setActiveTools` (types.ts:1320).
    fn set_active_tools(&self, tool_names: Vec<String>);

    /// `refreshTools` (types.ts:1556) — rebuild tool-dependent state after
    /// `registerTool` post-bind; a no-op pre-bind (loader.ts:191-192).
    fn refresh_tools(&self);

    /// `getCommands` (types.ts:1323) — `SlashCommandInfo[]` JSON.
    fn get_commands(&self) -> Vec<Value>;

    /// `setModel` (types.ts:1330). `model` is the `Model` JSON; returns
    /// false when no API key is available.
    async fn set_model(&self, model: Value) -> bool;

    /// `getThinkingLevel` / `setThinkingLevel` (types.ts:1333-1336); the
    /// level is the upstream `ThinkingLevel` string (host clamps).
    fn get_thinking_level(&self) -> String;
    fn set_thinking_level(&self, level: &str);

    /// `registerProvider(name, config)` post-bind direct call
    /// (runner.ts:387-393). `config` is the `ProviderConfig` JSON. `Err` is
    /// reported as a `"register_provider"` extension error (runner.ts:358-364).
    async fn register_provider(&self, name: &str, config: Value) -> Result<(), String>;

    /// `registerProvider(provider)` — the native-provider overload
    /// (loader.ts:374-382): a rpi-ai `Provider` trait object. Only native
    /// (L0) extensions can use it; there is no JSON shape.
    async fn register_native_provider(
        &self,
        provider: Arc<dyn rpi_ai::models::Provider>,
    ) -> Result<(), String>;

    /// `unregisterProvider` post-bind direct call (runner.ts:401-407).
    async fn unregister_provider(&self, name: &str);
}

// ============================================================================
// UI bridge (ExtensionUIContext, types.ts:130-281)
// ============================================================================

/// `ExtensionUIDialogOptions` (types.ts:95-100). `signal` is not
/// serializable; programmatic dismiss lands with the W4 bridges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiDialogOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// `notify` type parameter (types.ts:141).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyType {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "error")]
    Error,
}

/// `WorkingIndicatorOptions` (types.ts:115-120).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingIndicatorOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

/// `WidgetPlacement` (types.ts:103).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WidgetPlacement {
    #[serde(rename = "aboveEditor")]
    AboveEditor,
    #[serde(rename = "belowEditor")]
    BelowEditor,
}

/// `ExtensionWidgetOptions` (types.ts:106-109).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionWidgetOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<WidgetPlacement>,
}

/// `setWidget` content (types.ts:169-174): plain lines, or a declarative
/// component tree where upstream takes a component factory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WidgetContent {
    Lines(Vec<String>),
    Component(ComponentTree),
}

/// `TerminalInputHandler` result (types.ts:112).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consume: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Raw terminal input listener (types.ts:112).
pub type TerminalInputHandler = Arc<dyn Fn(String) -> Option<TerminalInputResult> + Send + Sync>;

/// `getAllThemes()` entry (types.ts:268).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<String>,
}

/// `setTheme` result (types.ts:274).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetThemeResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The 28 UI methods behind `ctx.ui` (`ExtensionUIContext`,
/// types.ts:130-281). Implemented per mode in W4 (`InteractiveUiBridge` /
/// `RpcUiBridge` / `NullUiBridge`); the W1 host leaves the slot empty and
/// [`ExtensionContext::ui`] returns [`ExtError::Unbound`].
///
/// Component-factory parameters (widget/footer/header/custom/editor) are
/// declarative [`ComponentTree`] JSON — the L0/L1-shared render protocol
/// (design §13 open item 3); `theme` values are theme JSON. The
/// `AutocompleteProviderFactory` / editor-factory closures of upstream have
/// no serializable equivalent and are carried as opaque descriptors until
/// W4 finalizes them.
#[async_trait::async_trait]
pub trait UiBridge: Send + Sync {
    /// `select` (types.ts:132).
    async fn select(
        &self,
        title: &str,
        options: &[String],
        opts: Option<UiDialogOptions>,
    ) -> Option<String>;

    /// `confirm` (types.ts:135).
    async fn confirm(&self, title: &str, message: &str, opts: Option<UiDialogOptions>) -> bool;

    /// `input` (types.ts:138).
    async fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        opts: Option<UiDialogOptions>,
    ) -> Option<String>;

    /// `notify` (types.ts:141).
    fn notify(&self, message: &str, kind: NotifyType);

    /// `onTerminalInput` (types.ts:144) — interactive mode only.
    fn on_terminal_input(&self, handler: TerminalInputHandler) -> Unsubscribe;

    /// `setStatus` (types.ts:147). `None` clears.
    fn set_status(&self, key: &str, text: Option<&str>);

    /// `setWorkingMessage` (types.ts:150). `None` restores the default.
    fn set_working_message(&self, message: Option<&str>);

    /// `setWorkingVisible` (types.ts:153).
    fn set_working_visible(&self, visible: bool);

    /// `setWorkingIndicator` (types.ts:163). `None` restores the default.
    fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>);

    /// `setHiddenThinkingLabel` (types.ts:166). `None` restores the default.
    fn set_hidden_thinking_label(&self, label: Option<&str>);

    /// `setWidget` (types.ts:169-174). `None` removes the widget.
    fn set_widget(
        &self,
        key: &str,
        content: Option<WidgetContent>,
        options: Option<ExtensionWidgetOptions>,
    );

    /// `setFooter` (types.ts:182-186). `None` restores the built-in footer.
    fn set_footer(&self, component: Option<ComponentTree>);

    /// `setHeader` (types.ts:189). `None` restores the built-in header.
    fn set_header(&self, component: Option<ComponentTree>);

    /// `setTitle` (types.ts:192).
    fn set_title(&self, title: &str);

    /// `custom` (types.ts:195-209): show a component with keyboard focus,
    /// resolving with its `done(result)` value (`None` = `undefined`).
    async fn custom(&self, component: ComponentTree, options: Option<Value>) -> Option<Value>;

    /// `pasteToEditor` (types.ts:212).
    fn paste_to_editor(&self, text: &str);

    /// `setEditorText` / `getEditorText` (types.ts:215-218).
    fn set_editor_text(&self, text: &str);
    fn get_editor_text(&self) -> String;

    /// `editor` (types.ts:221).
    async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String>;

    /// `addAutocompleteProvider` (types.ts:224). Opaque descriptor until W4.
    fn add_autocomplete_provider(&self, provider: Value);

    /// `setEditorComponent` / `getEditorComponent` (types.ts:259-262).
    fn set_editor_component(&self, component: Option<ComponentTree>);
    fn get_editor_component(&self) -> Option<ComponentTree>;

    /// `theme` getter (types.ts:265) — current theme JSON.
    fn theme(&self) -> Value;

    /// `getAllThemes` (types.ts:268).
    fn get_all_themes(&self) -> Vec<ThemeInfo>;

    /// `getTheme` (types.ts:271) — theme JSON without switching.
    fn get_theme(&self, name: &str) -> Option<Value>;

    /// `setTheme` (types.ts:274) — name or theme JSON.
    fn set_theme(&self, theme: Value) -> SetThemeResult;

    /// `getToolsExpanded` / `setToolsExpanded` (types.ts:277-280).
    fn get_tools_expanded(&self) -> bool;
    fn set_tools_expanded(&self, expanded: bool);

    /// Identity of the no-op bridge: upstream computes
    /// `hasUI = uiContext !== noOpUIContext` (runner.ts:438-440), so the
    /// null bridge must be recognizable. Real bridges leave this false.
    fn is_noop(&self) -> bool {
        false
    }

    /// Downcast support for L0 built-in extensions that legitimately need
    /// the concrete bridge (the llama.cpp manager mounts its native TUI
    /// view through `InteractiveUiBridge`). Never useful across L1.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

// ============================================================================
// Context actions (ExtensionContextActions / ExtensionCommandContextActions,
// types.ts:1612-1654)
// ============================================================================

/// `ContextUsage` (types.ts:287-293).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// Estimated context tokens; `None` right after compaction before the
    /// next LLM response.
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// `CompactOptions` (types.ts:295-299). The completion/error callbacks are
/// host-side closures (upstream `onComplete(result)` /
/// `onError(error)`).
#[derive(Clone, Default)]
pub struct CompactOptions {
    pub custom_instructions: Option<String>,
    pub on_complete: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    pub on_error: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

/// `ExtensionContextActions` (types.ts:1612-1628) — session-bound context
/// methods behind `ctx.*`. Unbound methods fall back to the upstream
/// runner defaults (runner.ts:275-285).
#[async_trait::async_trait]
pub trait ContextActions: Send + Sync {
    /// Current model JSON (`Model` shape), `None` when unset.
    fn get_model(&self) -> Option<Value>;
    fn is_idle(&self) -> bool;
    fn is_project_trusted(&self) -> bool;
    fn get_signal(&self) -> Option<tokio_util::sync::CancellationToken>;
    fn abort(&self);
    fn has_pending_messages(&self) -> bool;
    /// Mode-specific shutdown (docs/extensions.md:1018-1034: interactive/rpc
    /// defer to idle; print is a no-op).
    fn shutdown(&self);
    fn get_context_usage(&self) -> Option<ContextUsage>;
    /// Fire-and-forget compaction trigger (agent-session.ts:2423-2433).
    fn compact(&self, options: CompactOptions);
    fn get_system_prompt(&self) -> String;
    /// `getSystemPromptOptions` (types.ts:1627) — `BuildSystemPromptOptions`
    /// JSON.
    fn get_system_prompt_options(&self) -> Value;
}

/// `withSession` callback (types.ts:358): receives the
/// [`ReplacedSessionContext`] of the replacement session.
pub type WithSessionFn =
    Arc<dyn Fn(ReplacedSessionContext) -> BoxFuture<'static, ()> + Send + Sync>;

/// `navigateTree` options (types.ts:368-371).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateTreeOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarize: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `ExtensionCommandContextActions` (types.ts:1630-1654): session-control
/// methods only safe in user-initiated command handlers. `newSession`'s
/// `setup` callback is not representable across the host boundary (it
/// receives a rpi `SessionManager`) — v1 deviation: use `with_session`.
/// Unbound methods return the upstream defaults (runner.ts:421-427):
/// `{cancelled: false}` / no-op.
#[async_trait::async_trait]
pub trait CommandContextActions: Send + Sync {
    async fn wait_for_idle(&self);
    /// Returns `cancelled`.
    async fn new_session(
        &self,
        parent_session: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> bool;
    /// `position`: `"before" | "at"`; `"at"` = clone. Returns `cancelled`.
    async fn fork(
        &self,
        entry_id: &str,
        position: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> bool;
    /// Returns `cancelled`.
    async fn navigate_tree(&self, target_id: &str, options: NavigateTreeOptions) -> bool;
    /// Returns `cancelled`.
    async fn switch_session(&self, session_path: &str, with_session: Option<WithSessionFn>)
        -> bool;
    async fn reload(&self);
}

// ============================================================================
// Shared runtime (ExtensionRuntime, types.ts:1566-1660)
// ============================================================================

/// Default stale message (loader.ts:202-204 / runner.ts:540).
pub const DEFAULT_STALE_MESSAGE: &str = "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";

/// Message of the throwing action stubs (loader.ts:171-173).
pub const NOT_INITIALIZED_MESSAGE: &str =
    "Extension runtime not initialized. Action methods cannot be called during extension loading.";

/// A provider registration queued before the host bound actions
/// (`pendingProviderRegistrations`, types.ts:1573).
#[derive(Debug, Clone)]
pub struct PendingProviderRegistration {
    pub name: String,
    /// `ProviderConfig` JSON.
    pub config: Value,
    pub extension_path: String,
}

/// A native-provider registration queued before bind
/// (`pendingNativeProviderRegistrations`, types.ts:1575).
pub struct PendingNativeProviderRegistration {
    pub provider: Arc<dyn rpi_ai::models::Provider>,
    pub extension_path: String,
}

/// Shared state created by the loader, bound by the host
/// (`ExtensionRuntime`, types.ts:1660 = state + actions). Cloning shares the
/// underlying state (the upstream runtime is a single mutable object every
/// extension API references).
#[derive(Clone)]
pub struct ExtensionRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    flag_values: RwLock<HashMap<String, FlagValue>>,
    pending_provider_registrations: RwLock<Vec<PendingProviderRegistration>>,
    pending_native_provider_registrations: RwLock<Vec<PendingNativeProviderRegistration>>,
    stale_message: RwLock<Option<String>>,
    actions: RwLock<Option<Arc<dyn HostActions>>>,
    context_actions: RwLock<Option<Arc<dyn ContextActions>>>,
    command_actions: RwLock<Option<Arc<dyn CommandContextActions>>>,
    ui_bridge: RwLock<Option<Arc<dyn UiBridge>>>,
    mode: RwLock<ExtensionMode>,
    event_bus: EventBus,
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRuntime {
    /// `createExtensionRuntime` (loader.ts:170-223): actions/UI unbound,
    /// mode defaults to `"print"` (runner.ts:270).
    pub fn new() -> Self {
        ExtensionRuntime {
            inner: Arc::new(RuntimeInner {
                flag_values: RwLock::new(HashMap::new()),
                pending_provider_registrations: RwLock::new(Vec::new()),
                pending_native_provider_registrations: RwLock::new(Vec::new()),
                stale_message: RwLock::new(None),
                actions: RwLock::new(None),
                context_actions: RwLock::new(None),
                command_actions: RwLock::new(None),
                ui_bridge: RwLock::new(None),
                mode: RwLock::new(ExtensionMode::Print),
                event_bus: EventBus::new(),
            }),
        }
    }

    /// `assertActive` (loader.ts:175-179).
    pub fn assert_active(&self) -> Result<(), ExtError> {
        match read(&self.inner.stale_message).clone() {
            Some(message) => Err(ExtError::Stale(message)),
            None => Ok(()),
        }
    }

    /// `invalidate` (loader.ts:201-205): first message wins.
    pub fn invalidate(&self, message: Option<String>) {
        let mut stale = write(&self.inner.stale_message);
        if stale.is_none() {
            *stale = Some(message.unwrap_or_else(|| DEFAULT_STALE_MESSAGE.to_owned()));
        }
    }

    pub fn is_stale(&self) -> bool {
        read(&self.inner.stale_message).is_some()
    }

    // -- flag values (types.ts:1571) ----------------------------------------

    pub fn flag_values(&self) -> HashMap<String, FlagValue> {
        read(&self.inner.flag_values).clone()
    }

    pub fn get_flag_value(&self, name: &str) -> Option<FlagValue> {
        read(&self.inner.flag_values).get(name).cloned()
    }

    pub fn set_flag_value(&self, name: &str, value: FlagValue) {
        write(&self.inner.flag_values).insert(name.to_owned(), value);
    }

    /// Set the registration default when no value exists (loader.ts:280-282).
    pub fn set_flag_default(&self, name: &str, default: FlagValue) {
        let mut values = write(&self.inner.flag_values);
        values.entry(name.to_owned()).or_insert(default);
    }

    // -- provider registration queue (loader.ts:208-219) --------------------

    /// Pre-bind: queue the registration. Post-bind: direct call; a failure
    /// surfaces as `Err` for the caller (the API layer) to report.
    pub async fn register_provider(
        &self,
        name: &str,
        config: Value,
        extension_path: &str,
    ) -> Result<(), String> {
        if let Some(actions) = self.actions() {
            return actions.register_provider(name, config).await;
        }
        write(&self.inner.pending_provider_registrations).push(PendingProviderRegistration {
            name: name.to_owned(),
            config,
            extension_path: extension_path.to_owned(),
        });
        Ok(())
    }

    /// Native-provider overload (loader.ts:374-382): same queue-then-direct
    /// pattern as [`Self::register_provider`].
    pub async fn register_native_provider(
        &self,
        provider: Arc<dyn rpi_ai::models::Provider>,
        extension_path: &str,
    ) -> Result<(), String> {
        if let Some(actions) = self.actions() {
            return actions.register_native_provider(provider).await;
        }
        write(&self.inner.pending_native_provider_registrations).push(
            PendingNativeProviderRegistration {
                provider,
                extension_path: extension_path.to_owned(),
            },
        );
        Ok(())
    }

    /// Pre-bind: drop queued registrations for `name` (loader.ts:214-219).
    /// Post-bind: direct call (runner.ts:401-407).
    pub async fn unregister_provider(&self, name: &str) {
        if let Some(actions) = self.actions() {
            actions.unregister_provider(name).await;
            return;
        }
        write(&self.inner.pending_provider_registrations)
            .retain(|registration| registration.name != name);
        write(&self.inner.pending_native_provider_registrations)
            .retain(|registration| registration.provider.id() != name);
    }

    /// Queued registrations, drained by the host on bind (runner.ts:350-366).
    pub fn take_pending_provider_registrations(&self) -> Vec<PendingProviderRegistration> {
        std::mem::take(&mut *write(&self.inner.pending_provider_registrations))
    }

    /// Queued native registrations, drained on bind (runner.ts:367-383).
    pub fn take_pending_native_provider_registrations(
        &self,
    ) -> Vec<PendingNativeProviderRegistration> {
        std::mem::take(&mut *write(
            &self.inner.pending_native_provider_registrations,
        ))
    }

    // -- host binding (bindCore / setUIContext slots) ------------------------

    pub fn actions(&self) -> Option<Arc<dyn HostActions>> {
        read(&self.inner.actions).clone()
    }

    /// Context actions (`ExtensionContextActions` slot, bound by the
    /// session-side bridge).
    pub fn context_actions(&self) -> Option<Arc<dyn ContextActions>> {
        read(&self.inner.context_actions).clone()
    }

    pub fn set_context_actions(&self, actions: Option<Arc<dyn ContextActions>>) {
        *write(&self.inner.context_actions) = actions;
    }

    /// Command-context actions (`bindCommandContext`, runner.ts:410-427).
    pub fn command_actions(&self) -> Option<Arc<dyn CommandContextActions>> {
        read(&self.inner.command_actions).clone()
    }

    pub fn set_command_actions(&self, actions: Option<Arc<dyn CommandContextActions>>) {
        *write(&self.inner.command_actions) = actions;
    }

    pub fn bind_actions(&self, actions: Arc<dyn HostActions>) {
        *write(&self.inner.actions) = Some(actions);
    }

    pub fn ui_bridge(&self) -> Option<Arc<dyn UiBridge>> {
        read(&self.inner.ui_bridge).clone()
    }

    /// `setUIContext` (runner.ts:429-432). `None` restores the no-UI state.
    pub fn set_ui_bridge(&self, ui_bridge: Option<Arc<dyn UiBridge>>, mode: ExtensionMode) {
        *write(&self.inner.ui_bridge) = ui_bridge;
        *write(&self.inner.mode) = mode;
    }

    /// Drop the bridge, keep the mode (dispose paths).
    pub fn clear_ui_bridge(&self) {
        *write(&self.inner.ui_bridge) = None;
    }

    pub fn mode(&self) -> ExtensionMode {
        *read(&self.inner.mode)
    }

    /// `hasUI` (runner.ts:438-440): a real (non-null) bridge is bound.
    pub fn has_ui(&self) -> bool {
        read(&self.inner.ui_bridge)
            .as_ref()
            .is_some_and(|bridge| !bridge.is_noop())
    }

    pub fn event_bus(&self) -> EventBus {
        self.inner.event_bus.clone()
    }

    /// Action access for [`ExtensionApi`] methods — the throwing stub
    /// (loader.ts:171-173).
    pub fn require_actions(&self) -> Result<Arc<dyn HostActions>, ExtError> {
        self.actions()
            .ok_or_else(|| ExtError::Unbound(NOT_INITIALIZED_MESSAGE.to_owned()))
    }
}

// ============================================================================
// Extension context (ExtensionContext, types.ts:306-341) — W1 subset
// ============================================================================

/// Context passed to event handlers and tool execution (`ExtensionContext`,
/// types.ts:306-341). Session-bound methods delegate to the bound
/// [`ContextActions`]; unbound they return the upstream runner defaults
/// (runner.ts:275-285). Every accessor asserts active first, mirroring the
/// guarded getters of `createContext()` (runner.ts:665-738).
#[derive(Clone)]
pub struct ExtensionContext {
    runtime: ExtensionRuntime,
    cwd: String,
    /// `before_agent_start` chains system-prompt replacements and exposes
    /// the current value through `ctx.getSystemPrompt()`
    /// (runner.ts:1075-1082).
    system_prompt_override: Option<Arc<RwLock<String>>>,
}

impl ExtensionContext {
    pub(crate) fn new(runtime: ExtensionRuntime, cwd: String) -> Self {
        ExtensionContext {
            runtime,
            cwd,
            system_prompt_override: None,
        }
    }

    /// Context with the `getSystemPrompt` override (runner.ts:1079-1082).
    pub(crate) fn with_system_prompt_override(
        runtime: ExtensionRuntime,
        cwd: String,
        current: Arc<RwLock<String>>,
    ) -> Self {
        ExtensionContext {
            runtime,
            cwd,
            system_prompt_override: Some(current),
        }
    }

    fn context_actions(&self) -> Result<Option<Arc<dyn ContextActions>>, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.context_actions())
    }

    pub fn runtime(&self) -> ExtensionRuntime {
        self.runtime.clone()
    }

    /// `ctx.mode` (runner.ts:673-676).
    pub fn mode(&self) -> Result<ExtensionMode, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.mode())
    }

    /// `ctx.hasUI` (runner.ts:677-680).
    pub fn has_ui(&self) -> Result<bool, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.has_ui())
    }

    /// `ctx.cwd` (runner.ts:681-684).
    pub fn cwd(&self) -> Result<&str, ExtError> {
        self.runtime.assert_active()?;
        Ok(&self.cwd)
    }

    /// `ctx.ui` (runner.ts:669-672). Upstream defaults to `noOpUIContext`
    /// rather than throwing (runner.ts:269), so an unbound slot yields a
    /// shared [`crate::bridges::NullUiBridge`].
    pub fn ui(&self) -> Result<Arc<dyn UiBridge>, ExtError> {
        self.runtime.assert_active()?;
        Ok(self
            .runtime
            .ui_bridge()
            .unwrap_or_else(|| crate::bridges::NullUiBridge::shared()))
    }

    /// `pi.events` — the shared bus, also reachable from handlers.
    pub fn events(&self) -> EventBus {
        self.runtime.event_bus()
    }

    // -- session-bound accessors (types.ts:324-341; defaults runner.ts:275-285)

    /// `ctx.model` (runner.ts:693-696) — current model JSON.
    pub fn model(&self) -> Result<Option<Value>, ExtError> {
        Ok(self
            .context_actions()?
            .and_then(|actions| actions.get_model()))
    }

    /// `ctx.isIdle()` — default true when unbound (runner.ts:276).
    pub fn is_idle(&self) -> Result<bool, ExtError> {
        Ok(self
            .context_actions()?
            .is_none_or(|actions| actions.is_idle()))
    }

    /// `ctx.isProjectTrusted()` — default true when unbound.
    pub fn is_project_trusted(&self) -> Result<bool, ExtError> {
        Ok(self
            .context_actions()?
            .is_none_or(|actions| actions.is_project_trusted()))
    }

    /// `ctx.signal` (runner.ts:711-713) — `None` when idle/unbound.
    pub fn signal(&self) -> Result<Option<tokio_util::sync::CancellationToken>, ExtError> {
        Ok(self
            .context_actions()?
            .and_then(|actions| actions.get_signal()))
    }

    /// `ctx.abort()` (runner.ts:713-716).
    pub fn abort(&self) -> Result<(), ExtError> {
        if let Some(actions) = self.context_actions()? {
            actions.abort();
        }
        Ok(())
    }

    /// `ctx.hasPendingMessages()` — default false when unbound.
    pub fn has_pending_messages(&self) -> Result<bool, ExtError> {
        Ok(self
            .context_actions()?
            .is_some_and(|actions| actions.has_pending_messages()))
    }

    /// `ctx.shutdown()` (mode-specific behavior rides the bound handler).
    pub fn shutdown(&self) -> Result<(), ExtError> {
        if let Some(actions) = self.context_actions()? {
            actions.shutdown();
        }
        Ok(())
    }

    /// `ctx.getContextUsage()` — default `None` when unbound.
    pub fn get_context_usage(&self) -> Result<Option<ContextUsage>, ExtError> {
        Ok(self
            .context_actions()?
            .and_then(|actions| actions.get_context_usage()))
    }

    /// `ctx.compact(options?)` — fire-and-forget
    /// (agent-session.ts:2423-2433); no-op when unbound.
    pub fn compact(&self, options: CompactOptions) -> Result<(), ExtError> {
        if let Some(actions) = self.context_actions()? {
            actions.compact(options);
        }
        Ok(())
    }

    /// `ctx.getSystemPrompt()`. During `before_agent_start` this reflects
    /// the chained per-turn replacement (runner.ts:1079-1082); otherwise the
    /// session's effective prompt. Default "" when unbound.
    pub fn get_system_prompt(&self) -> Result<String, ExtError> {
        self.runtime.assert_active()?;
        if let Some(override_) = &self.system_prompt_override {
            return Ok(read(override_).clone());
        }
        Ok(self
            .runtime
            .context_actions()
            .map_or_else(String::new, |actions| actions.get_system_prompt()))
    }
}

// ============================================================================
// Command / replaced-session contexts (types.ts:347-398)
// ============================================================================

/// `ExtensionCommandContext` (types.ts:347-381) — base context plus
/// session-control methods for command handlers. Unbound methods return the
/// upstream defaults (runner.ts:421-427).
#[derive(Clone)]
pub struct ExtensionCommandContext {
    base: ExtensionContext,
}

impl ExtensionCommandContext {
    pub(crate) fn new(base: ExtensionContext) -> Self {
        ExtensionCommandContext { base }
    }

    /// The base event-handler context.
    pub fn base(&self) -> &ExtensionContext {
        &self.base
    }

    fn command_actions(&self) -> Result<Option<Arc<dyn CommandContextActions>>, ExtError> {
        self.base.runtime.assert_active()?;
        Ok(self.base.runtime.command_actions())
    }

    /// `getSystemPromptOptions` (types.ts:349). Default `{cwd}` when
    /// unbound (runner.ts:285, 347).
    pub fn get_system_prompt_options(&self) -> Result<Value, ExtError> {
        Ok(self
            .base
            .runtime
            .context_actions()
            .map(|actions| actions.get_system_prompt_options())
            .unwrap_or_else(|| serde_json::json!({ "cwd": self.base.cwd })))
    }

    pub async fn wait_for_idle(&self) -> Result<(), ExtError> {
        if let Some(actions) = self.command_actions()? {
            actions.wait_for_idle().await;
        }
        Ok(())
    }

    /// `newSession` (types.ts:355-359). Returns `cancelled`.
    pub async fn new_session(
        &self,
        parent_session: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> Result<bool, ExtError> {
        match self.command_actions()? {
            Some(actions) => Ok(actions.new_session(parent_session, with_session).await),
            None => Ok(false),
        }
    }

    /// `fork` (types.ts:362-365). `position`: `"before" | "at"`. Returns
    /// `cancelled`.
    pub async fn fork(
        &self,
        entry_id: &str,
        position: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> Result<bool, ExtError> {
        match self.command_actions()? {
            Some(actions) => Ok(actions.fork(entry_id, position, with_session).await),
            None => Ok(false),
        }
    }

    /// `navigateTree` (types.ts:368-371). Returns `cancelled`.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<bool, ExtError> {
        match self.command_actions()? {
            Some(actions) => Ok(actions.navigate_tree(target_id, options).await),
            None => Ok(false),
        }
    }

    /// `switchSession` (types.ts:374-377). Returns `cancelled`.
    pub async fn switch_session(
        &self,
        session_path: &str,
        with_session: Option<WithSessionFn>,
    ) -> Result<bool, ExtError> {
        match self.command_actions()? {
            Some(actions) => Ok(actions.switch_session(session_path, with_session).await),
            None => Ok(false),
        }
    }

    /// `reload` (types.ts:380).
    pub async fn reload(&self) -> Result<(), ExtError> {
        if let Some(actions) = self.command_actions()? {
            actions.reload().await;
        }
        Ok(())
    }
}

impl std::ops::Deref for ExtensionCommandContext {
    type Target = ExtensionContext;
    fn deref(&self) -> &ExtensionContext {
        &self.base
    }
}

/// `ReplacedSessionContext` (types.ts:388-398): a command-capable context
/// bound to the replacement session, plus `sendMessage`/`sendUserMessage`
/// routing into the NEW session's bound actions (agent-session.ts:3304-3312).
pub struct ReplacedSessionContext {
    command_context: ExtensionCommandContext,
    runtime: ExtensionRuntime,
}

impl ReplacedSessionContext {
    /// Built by the rpi side after a session replacement (the new
    /// session's command context + runtime).
    pub fn new(command_context: ExtensionCommandContext, runtime: ExtensionRuntime) -> Self {
        ReplacedSessionContext {
            command_context,
            runtime,
        }
    }

    pub fn command_context(&self) -> &ExtensionCommandContext {
        &self.command_context
    }

    /// `sendMessage` on the replacement session (types.ts:389-392).
    pub fn send_message(
        &self,
        message: Value,
        options: Option<SendMessageOptions>,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .require_actions()?
            .send_message(message, options);
        Ok(())
    }

    /// `sendUserMessage` on the replacement session (types.ts:394-397).
    pub fn send_user_message(
        &self,
        content: Value,
        options: Option<SendUserMessageOptions>,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .require_actions()?
            .send_user_message(content, options);
        Ok(())
    }
}

// ============================================================================
// Loaded extension (Extension, types.ts:1662-1675)
// ============================================================================

/// JSON-crossing event handler: receives the event payload (camelCase JSON
/// with the `type` tag stamped), returns the handler result (`Value::Null`
/// = upstream `undefined` / void). `Err(message)` mirrors a thrown error.
pub type EventHandler =
    Arc<dyn Fn(Value, ExtensionContext) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

/// `Extension` (types.ts:1662-1675). All collections are interior-mutable:
/// registration methods may run after load (e.g. `registerTool` from an
/// event handler, loader.ts:245-252), and the runner observes them live.
pub struct LoadedExtension {
    pub path: String,
    pub resolved_path: String,
    hidden: std::sync::atomic::AtomicBool,
    pub source_info: ExtSourceInfo,
    handlers: RwLock<HashMap<String, Vec<EventHandler>>>,
    tools: RwLock<InsertionMap<RegisteredTool>>,
    message_renderers: RwLock<InsertionMap<MessageRenderFn>>,
    entry_renderers: RwLock<InsertionMap<EntryRenderFn>>,
    commands: RwLock<InsertionMap<RegisteredCommand>>,
    flags: RwLock<InsertionMap<ExtensionFlag>>,
    shortcuts: RwLock<InsertionMap<ExtensionShortcut>>,
    /// The live wasm guest backing this extension (L1; T15 W6). Owns the
    /// guest thread; dropping the extension shuts it down.
    wasm_guest: RwLock<Option<crate::wasm::WasmGuest>>,
    /// The loaded native plugin backing this extension (L0 dynamic
    /// library, T15 W7); keeps the library mapped.
    native_plugin: RwLock<Option<crate::native::NativePlugin>>,
}

impl LoadedExtension {
    /// `createExtension` (loader.ts:433-452): synthetic source info; the
    /// source of `<...>` paths is the inner text before `:` (default
    /// `"temporary"`), everything else is `"local"` with a `base_dir`.
    pub fn new(extension_path: &str, resolved_path: &str) -> Self {
        let is_synthetic = extension_path.starts_with('<') && extension_path.ends_with('>');
        let (source, base_dir) = if is_synthetic {
            let inner = &extension_path[1..extension_path.len() - 1];
            let source = inner.split(':').next().unwrap_or("temporary");
            let source = if source.is_empty() {
                "temporary"
            } else {
                source
            };
            (source.to_owned(), None)
        } else {
            let base_dir = std::path::Path::new(resolved_path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned());
            ("local".to_owned(), base_dir)
        };
        LoadedExtension {
            path: extension_path.to_owned(),
            resolved_path: resolved_path.to_owned(),
            hidden: std::sync::atomic::AtomicBool::new(false),
            source_info: ExtSourceInfo::synthetic(extension_path, &source, base_dir),
            handlers: RwLock::new(HashMap::new()),
            tools: RwLock::new(InsertionMap::new()),
            message_renderers: RwLock::new(InsertionMap::new()),
            entry_renderers: RwLock::new(InsertionMap::new()),
            commands: RwLock::new(InsertionMap::new()),
            flags: RwLock::new(InsertionMap::new()),
            shortcuts: RwLock::new(InsertionMap::new()),
            wasm_guest: RwLock::new(None),
            native_plugin: RwLock::new(None),
        }
    }

    /// `hidden` flag (resource-loader.ts:905): named inline extensions may
    /// opt out of the startup Extensions list.
    pub fn hidden(&self) -> bool {
        self.hidden.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn set_hidden(&self, hidden: bool) {
        self.hidden
            .store(hidden, std::sync::atomic::Ordering::Relaxed);
    }

    /// Attach the guest instance after a wasm `rpi_extension_init`
    /// succeeds (T15 W6).
    pub fn set_wasm_guest(&self, guest: crate::wasm::WasmGuest) {
        *write(&self.wasm_guest) = Some(guest);
    }

    /// Attach a loaded native plugin (T15 W7).
    pub fn set_native_plugin(&self, plugin: crate::native::NativePlugin) {
        *write(&self.native_plugin) = Some(plugin);
    }

    /// The live wasm guest's dispatch handle (L1; T15 W6) — `None` for
    /// native-only extensions. Host-side code uses this for direct
    /// round trips (render paths).
    pub fn wasm_forward(&self) -> Option<crate::wasm::WasmForward> {
        read(&self.wasm_guest).as_ref().map(|guest| guest.forward())
    }

    pub(crate) fn push_handler(&self, event: &str, handler: EventHandler) {
        write(&self.handlers)
            .entry(event.to_owned())
            .or_default()
            .push(handler);
    }

    pub(crate) fn handlers_for(&self, event: &str) -> Vec<EventHandler> {
        read(&self.handlers).get(event).cloned().unwrap_or_default()
    }

    pub(crate) fn has_handlers(&self, event: &str) -> bool {
        read(&self.handlers)
            .get(event)
            .is_some_and(|handlers| !handlers.is_empty())
    }

    pub(crate) fn insert_tool(&self, tool: RegisteredTool) {
        write(&self.tools).set(tool.definition.name.clone(), tool);
    }

    pub fn tools(&self) -> InsertionMap<RegisteredTool> {
        read(&self.tools).clone()
    }

    pub(crate) fn insert_command(&self, command: RegisteredCommand) {
        write(&self.commands).set(command.name.clone(), command);
    }

    pub fn commands(&self) -> InsertionMap<RegisteredCommand> {
        read(&self.commands).clone()
    }

    pub(crate) fn insert_flag(&self, flag: ExtensionFlag) {
        write(&self.flags).set(flag.name.clone(), flag);
    }

    pub fn flags(&self) -> InsertionMap<ExtensionFlag> {
        read(&self.flags).clone()
    }

    pub(crate) fn insert_shortcut(&self, shortcut: ExtensionShortcut) {
        write(&self.shortcuts).set(shortcut.shortcut.clone(), shortcut);
    }

    pub fn shortcuts(&self) -> InsertionMap<ExtensionShortcut> {
        read(&self.shortcuts).clone()
    }

    pub(crate) fn insert_message_renderer(&self, custom_type: &str, renderer: MessageRenderFn) {
        write(&self.message_renderers).set(custom_type.to_owned(), renderer);
    }

    pub(crate) fn insert_entry_renderer(&self, custom_type: &str, renderer: EntryRenderFn) {
        write(&self.entry_renderers).set(custom_type.to_owned(), renderer);
    }

    pub fn message_renderer(&self, custom_type: &str) -> Option<MessageRenderFn> {
        read(&self.message_renderers).get(custom_type).cloned()
    }

    pub fn entry_renderer(&self, custom_type: &str) -> Option<EntryRenderFn> {
        read(&self.entry_renderers).get(custom_type).cloned()
    }
}

// ============================================================================
// Extension API (createExtensionAPI, loader.ts:230-393)
// ============================================================================

/// The `pi` object handed to extension factories (`ExtensionAPI`,
/// types.ts:1179-1414). Cheap to clone; clones share the extension
/// registration maps and the runtime (upstream closes over the same
/// objects).
#[derive(Clone)]
pub struct ExtensionApi {
    extension: Arc<LoadedExtension>,
    runtime: ExtensionRuntime,
    cwd: String,
}

impl ExtensionApi {
    pub(crate) fn new(
        extension: Arc<LoadedExtension>,
        runtime: ExtensionRuntime,
        cwd: String,
    ) -> Self {
        ExtensionApi {
            extension,
            runtime,
            cwd,
        }
    }

    /// Re-attach an API handle to an already-loaded extension (the upstream
    /// API object closes over the same extension/runtime, so this is
    /// equivalent; used by hosts handing `pi` to late-bound callbacks, and
    /// by tests).
    pub fn for_extension(
        extension: Arc<LoadedExtension>,
        runtime: ExtensionRuntime,
        cwd: &str,
    ) -> Self {
        Self::new(extension, runtime, cwd.to_owned())
    }

    /// The extension this API registers into.
    pub fn extension(&self) -> Arc<LoadedExtension> {
        self.extension.clone()
    }

    pub fn runtime(&self) -> ExtensionRuntime {
        self.runtime.clone()
    }

    /// The base extension context this API's extension sees
    /// (event-handler contexts come from the runner core,
    /// runner.ts:665).
    pub fn context(&self) -> ExtensionContext {
        ExtensionContext::new(self.runtime.clone(), self.cwd.clone())
    }

    // -- event subscription (loader.ts:238-243) ------------------------------

    /// `pi.on(event, handler)` — raw JSON handler (the L0/L1-shared shape).
    pub fn on(&self, event: &str, handler: EventHandler) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.push_handler(event, handler);
        Ok(())
    }

    /// Typed convenience wrapper over [`ExtensionApi::on`]: the payload is
    /// deserialized to `E`, the optional result serialized from `R`.
    /// Deserialization failure is reported as a handler error.
    pub fn on_typed<E, R, F, Fut>(&self, event: &str, handler: F) -> Result<(), ExtError>
    where
        E: serde::de::DeserializeOwned,
        R: Serialize,
        F: Fn(E, ExtensionContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<R>, String>> + Send + 'static,
    {
        let wrapped: EventHandler = Arc::new(move |payload, ctx| {
            let future = serde_json::from_value::<E>(payload)
                .map(|event| handler(event, ctx))
                .map_err(|err| format!("failed to deserialize event payload: {err}"));
            Box::pin(async move {
                let future = future?;
                match future.await? {
                    Some(result) => serde_json::to_value(result)
                        .map_err(|err| format!("failed to serialize handler result: {err}")),
                    None => Ok(Value::Null),
                }
            })
        });
        self.on(event, wrapped)
    }

    // -- registration (loader.ts:245-294) ------------------------------------

    /// `pi.registerTool(tool)` (loader.ts:245-252). Same-extension
    /// re-registration overwrites in place (JS `Map.set`); cross-extension
    /// conflicts resolve at query time (runner.ts:447-457).
    pub fn register_tool(&self, tool: ToolDefinition) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.insert_tool(RegisteredTool {
            definition: tool,
            source_info: self.extension.source_info.clone(),
        });
        // refreshTools is a no-op pre-bind (loader.ts:191-192).
        if let Some(actions) = self.runtime.actions() {
            actions.refresh_tools();
        }
        Ok(())
    }

    /// `pi.registerCommand(name, options)` (loader.ts:254-261).
    pub fn register_command(
        &self,
        name: &str,
        description: Option<String>,
        handler: CommandHandlerFn,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.insert_command(RegisteredCommand {
            name: name.to_owned(),
            source_info: self.extension.source_info.clone(),
            description,
            get_argument_completions: None,
            handler,
        });
        Ok(())
    }

    /// `registerCommand` with `getArgumentCompletions` (types.ts:1160).
    pub fn register_command_with_completions(
        &self,
        name: &str,
        description: Option<String>,
        get_argument_completions: Option<ArgumentCompletionsFn>,
        handler: CommandHandlerFn,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.insert_command(RegisteredCommand {
            name: name.to_owned(),
            source_info: self.extension.source_info.clone(),
            description,
            get_argument_completions,
            handler,
        });
        Ok(())
    }

    /// `pi.registerShortcut(shortcut, options)` (loader.ts:263-272). The key
    /// is stored normalized (lowercase), matching runner.ts:504.
    pub fn register_shortcut(
        &self,
        shortcut: &str,
        description: Option<String>,
        handler: ShortcutHandlerFn,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.insert_shortcut(ExtensionShortcut {
            shortcut: shortcut.to_lowercase(),
            description,
            handler,
            extension_path: self.extension.path.clone(),
        });
        Ok(())
    }

    /// `pi.registerFlag(name, options)` (loader.ts:274-283).
    pub fn register_flag(
        &self,
        name: &str,
        description: Option<String>,
        flag_type: FlagType,
        default: Option<FlagValue>,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.insert_flag(ExtensionFlag {
            name: name.to_owned(),
            description,
            flag_type,
            default: default.clone(),
            extension_path: self.extension.path.clone(),
        });
        if let Some(default) = default {
            self.runtime.set_flag_default(name, default);
        }
        Ok(())
    }

    /// `pi.getFlag(name)` (loader.ts:297-301): only flags registered by
    /// *this* extension are visible through its API.
    pub fn get_flag(&self, name: &str) -> Result<Option<FlagValue>, ExtError> {
        self.runtime.assert_active()?;
        if !self.extension.flags().contains(name) {
            return Ok(None);
        }
        Ok(self.runtime.get_flag_value(name))
    }

    /// `pi.registerMessageRenderer(customType, renderer)` (loader.ts:285-288).
    pub fn register_message_renderer(
        &self,
        custom_type: &str,
        renderer: MessageRenderFn,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension
            .insert_message_renderer(custom_type, renderer);
        Ok(())
    }

    /// `pi.registerEntryRenderer(customType, renderer)` (loader.ts:290-294).
    pub fn register_entry_renderer(
        &self,
        custom_type: &str,
        renderer: EntryRenderFn,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.extension.insert_entry_renderer(custom_type, renderer);
        Ok(())
    }

    // -- actions (loader.ts:304-387, delegated to HostActions) ---------------

    /// `pi.sendMessage(message, options)` (loader.ts:304-307).
    pub fn send_message(
        &self,
        message: Value,
        options: Option<SendMessageOptions>,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .require_actions()?
            .send_message(message, options);
        Ok(())
    }

    /// `pi.sendUserMessage(content, options)` (loader.ts:309-312).
    pub fn send_user_message(
        &self,
        content: Value,
        options: Option<SendUserMessageOptions>,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .require_actions()?
            .send_user_message(content, options);
        Ok(())
    }

    /// `pi.appendEntry(customType, data)` (loader.ts:314-317).
    pub fn append_entry(&self, custom_type: &str, data: Option<Value>) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .require_actions()?
            .append_entry(custom_type, data);
        Ok(())
    }

    /// `pi.setSessionName(name)` (loader.ts:319-322).
    pub fn set_session_name(&self, name: &str) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime.require_actions()?.set_session_name(name);
        Ok(())
    }

    /// `pi.getSessionName()` (loader.ts:324-327).
    pub fn get_session_name(&self) -> Result<Option<String>, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.require_actions()?.get_session_name())
    }

    /// `pi.setLabel(entryId, label)` (loader.ts:329-332).
    pub fn set_label(&self, entry_id: &str, label: Option<&str>) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime.require_actions()?.set_label(entry_id, label);
        Ok(())
    }

    /// `pi.exec(command, args, options)` (loader.ts:334-337).
    pub async fn exec(
        &self,
        command: &str,
        args: &[String],
        options: Option<ExecOptions>,
    ) -> Result<ExecResult, ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .require_actions()?
            .exec(command, args, options)
            .await
    }

    /// `pi.getActiveTools()` (loader.ts:339-342).
    pub fn get_active_tools(&self) -> Result<Vec<String>, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.require_actions()?.get_active_tools())
    }

    /// `pi.getAllTools()` (loader.ts:344-347) — `ToolInfo[]` JSON.
    pub fn get_all_tools(&self) -> Result<Vec<Value>, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.require_actions()?.get_all_tools())
    }

    /// `pi.setActiveTools(toolNames)` (loader.ts:349-352).
    pub fn set_active_tools(&self, tool_names: Vec<String>) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime.require_actions()?.set_active_tools(tool_names);
        Ok(())
    }

    /// `pi.getCommands()` (loader.ts:354-357) — `SlashCommandInfo[]` JSON.
    pub fn get_commands(&self) -> Result<Vec<Value>, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.require_actions()?.get_commands())
    }

    /// `pi.setModel(model)` (loader.ts:359-362). `model` is the `Model` JSON.
    pub async fn set_model(&self, model: Value) -> Result<bool, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.require_actions()?.set_model(model).await)
    }

    /// `pi.getThinkingLevel()` (loader.ts:364-367).
    pub fn get_thinking_level(&self) -> Result<String, ExtError> {
        self.runtime.assert_active()?;
        Ok(self.runtime.require_actions()?.get_thinking_level())
    }

    /// `pi.setThinkingLevel(level)` (loader.ts:369-372).
    pub fn set_thinking_level(&self, level: &str) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime.require_actions()?.set_thinking_level(level);
        Ok(())
    }

    /// `pi.registerProvider(name, config)` (loader.ts:374-382, name+config
    /// signature).
    pub async fn register_provider(&self, name: &str, config: Value) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .register_provider(name, config, &self.extension.path)
            .await
            .map_err(ExtError::Call)?;
        Ok(())
    }

    /// `pi.registerProvider(provider)` — native-provider overload
    /// (loader.ts:380-381).
    pub async fn register_native_provider(
        &self,
        provider: Arc<dyn rpi_ai::models::Provider>,
    ) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime
            .register_native_provider(provider, &self.extension.path)
            .await
            .map_err(ExtError::Call)?;
        Ok(())
    }

    /// `pi.unregisterProvider(name)` (loader.ts:384-387).
    pub async fn unregister_provider(&self, name: &str) -> Result<(), ExtError> {
        self.runtime.assert_active()?;
        self.runtime.unregister_provider(name).await;
        Ok(())
    }

    /// `pi.events` (loader.ts:389).
    pub fn events(&self) -> EventBus {
        self.runtime.event_bus()
    }
}
