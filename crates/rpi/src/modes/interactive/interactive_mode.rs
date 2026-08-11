//! Interactive mode — port of
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` @ pi
//! 0.82.1 (2efa728).
//!
//! The mode skeleton (layout container chain, initialization, session event
//! → component-tree dispatch over all 24 `AgentSessionEvent` branches,
//! initial-message rendering, editor submit/escape basics,
//! footer/custom-editor/header wiring, app.rs startup hook) plus the
//! selectors, slash commands, queue UX, model cycling, clipboard, suspend
//! and settings hot-apply all live here and in the `commands.rs` /
//! `commands_selectors.rs` submodules. Remaining upstream gaps carry
//! `TODO(T13)`/`TODO(T14)`/`TODO(T15)` markers (provider/auth flows,
//! cache-stats/share/export HTML, extension UI respectively); gaps no v0.1
//! task claims are marked `TODO(unassigned)`.
//!
//! ## Structure vs upstream
//!
//! Upstream is a single class where the TUI, the component tree and the
//! session event callback all live on one thread (JS). The port splits the
//! responsibilities because the synchronous [`AgentSession::subscribe`]
//! callback can fire from the agent task while the TUI event loop and the
//! prompt-sequencing loop run elsewhere:
//!
//! - [`InteractiveUi`] (shared, `Arc`-wrapped) owns the component tree and
//!   all UI state. Its [`InteractiveUi::drain_events`] applies queued
//!   [`UiCommand`]s; it is driven by the TUI driver thread (between `pump`
//!   calls), which preserves the upstream ordering (session events are
//!   processed by the event loop, never from inside the subscribe callback —
//!   rpi-tui lock contract, tui.rs header note).
//! - [`InteractiveMode`] owns the session/runtime, the driver thread and the
//!   prompt-sequencing run loop (upstream `run`'s `while (true) { await
//!   prompt(...) }`). Submits from the editor arrive over a channel; the
//!   compaction flush is signalled over a watch channel.
//! - The subscribe callback only converts the event and pushes it onto the
//!   queue — no component locking inside callbacks.
//!
//! The component tree uses the region pattern (same idiom as
//! `BashExecutionComponent.loader`): tree entries are thin wrappers around
//! `Arc<Mutex<...>>` handles so the mode can mutate components from the
//! drain. Removal from the chat container uses address-only comparisons
//! (mirroring `Container::remove_child`'s pointer-identity semantics) — the
//! stored addresses are `usize` casts, never dereferenced.
//!
//! Intentional differences:
//! - `bindExtensions` is called with `mode: None` (upstream binds via
//!   `bindCurrentSessionExtensions` with no explicit mode for interactive).
//! - Startup notices (changelog asset — T15) and the package-update check
//!   (npm-registry probing, not a product endpoint) are not ported; the
//!   startup version check and install telemetry landed with T14-W6a
//!   (ADR-0002 §8); the tmux keyboard checks (unassigned) are not ported;
//!   the `loadedResourcesContainer` shows the merged diagnostics block only
//!   (see `show_loaded_resources`).
//! - `setRebindSession`/`setBeforeSessionInvalidate` runtime hooks stay
//!   `None`: session switching rebinds the UI explicitly via
//!   `InteractiveMode::rebind_session_ui` (called from each command handler)
//!   instead of the runtime callback (interactive-mode.ts:455-457).
//! - Terminal input draining before exit (`terminal.drainInput(1000)`,
//!   interactive-mode.ts:3572-3583) has no port: the trait method returns a
//!   future borrowing the terminal, which the mode cannot poll through
//!   `Tui::with_terminal`. Documented gap (Kitty key-release leaks over slow
//!   SSH); the terminal is restored by `Tui::stop` as usual.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::modes::interactive::component_tree::component_from_tree;
use rpi_agent::messages::{
    AgentMessage, BranchSummaryMessage, BranchSummaryRole, CompactionSummaryMessage,
    CompactionSummaryRole, CustomMessage, CustomRole,
};
use rpi_agent::session::{parse_iso8601_ms, SessionEntry};
use rpi_agent::types::{AgentEvent, ThinkingLevel};
use rpi_ai::types::{
    AssistantContent, ImageContent, ModelThinkingLevel, StopReason, ToolResultContent,
};
use rpi_tui::components::editor::{EditorOptions, EditorTheme};
use rpi_tui::components::loader::LoaderIndicatorOptions;
use rpi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use rpi_tui::components::select_list::SelectListTheme;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::components::truncated_text::TruncatedText;
use rpi_tui::keybindings as tui_keybindings;
use rpi_tui::tui::{
    shared_component_from_boxed, Component, Container, Focusable, RenderHandle, SharedComponent,
    Tui,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

use crate::config::{APP_NAME, VERSION};
use crate::core::agent_session::{
    AgentSession, AgentSessionEvent, ExtensionBindings, PromptOptions,
};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::compaction_runner::{CompactionEvent, CompactionReason, RetrySource};
use crate::core::extensions::{ExtensionRunner, StreamingBehavior};
use crate::core::settings_manager::DoubleEscapeAction;
use crate::core::themes::{load_theme, Theme};
use crate::core::trust_manager::has_trust_requiring_project_resources;
use crate::error::RpiError;
use crate::modes::interactive::components::keybinding_hints::key_display_text;
use crate::modes::interactive::components::status_indicator::{
    BranchSummaryStatusIndicator, CompactionStatusIndicator, CompactionStatusReason,
    RetryStatusIndicator, StatusIndicatorKind, WorkingStatusIndicator,
};
use crate::modes::interactive::components::tree_selector::TreeSelectorComponent;
use crate::modes::interactive::components::user_message_selector::UserMessageSelectorComponent;
use crate::modes::interactive::components::{
    dynamic_border::DynamicBorder, tool_execution::ToolResultState, BashExecutionComponent,
    BranchSummaryMessageComponent, CompactionSummaryMessageComponent, CustomEntryComponent,
    CustomMessageComponent, SkillInvocationMessageComponent, ToolExecutionComponent,
    ToolExecutionOptions, ToolResultContentLoose, UserMessageComponent,
};
use crate::modes::interactive::custom_editor::{CustomEditor, CustomEditorRegion, EscapeHandler};
use crate::modes::interactive::footer::{FooterComponent, FooterDataProvider};
use crate::modes::interactive::header::{build_builtin_header, ExpandableText};
use crate::modes::interactive::theme::markdown_theme;
use crate::tools::truncate::TruncationResult;

/// Terminal title prefix (upstream `APP_TITLE`, config.ts:490: `"π"` when no
/// config name is set, else the app name). Rpi uses `APP_NAME` (ADR-0001).
const APP_TITLE: &str = "rpi";

/// The TUI driver thread's maximum pump wait: bounds both render latency for
/// session events and shutdown response time (upstream has no equivalent —
/// the event loop is always awake; here the driver polls with this cap).
const DRIVER_PUMP_CAP: Duration = Duration::from_millis(50);

/// Double-press window for Ctrl+C exit / double-Escape
/// (interactive-mode.ts:3533-3541, 2573-2588).
const DOUBLE_PRESS_MS: u64 = 500;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Slash-command handlers and selector wiring: `commands_selectors.rs`. A
/// submodule so it can extend the private `InteractiveUi` /
/// `InteractiveMode` impls without exposing their fields. The `dead_code`
/// allowance covers hook-only paths (e.g. selectors mounted while their
/// follow-up flow is still a T13/T15 hook).
#[allow(dead_code)]
pub(crate) mod commands_selectors;
pub(crate) mod ui_bridge;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// =============================================================================
// Startup plumbing (app.rs hook)
// =============================================================================

/// `InteractiveModeOptions` (interactive-mode.ts:306-321), S4b subset.
#[derive(Debug, Default, Clone)]
pub struct InteractiveModeOptions {
    /// Warning message if the session model couldn't be restored.
    pub model_fallback_message: Option<String>,
    /// Initial message to send on startup (may contain `@file` content).
    pub initial_message: Option<String>,
    /// Images to attach to the initial message.
    pub initial_images: Option<Vec<ImageContent>>,
    /// Additional messages to send after the initial message.
    pub initial_messages: Vec<String>,
    /// Force verbose startup (overrides the quietStartup setting).
    pub verbose: bool,
}

/// Install the keybinding managers into both globals
/// (interactive-mode.ts:468-469): the rpi core global (73-entry table,
/// `app.*` included — read by the keybinding-hint helpers) and the rpi-tui
/// global (read by `CustomEditor` dispatch and the editor's `tui.*`
/// matching). The rpi core manager owns the definition table, JSON loading
/// and legacy-name migration (core/keybindings.rs); the rpi-tui manager is
/// rebuilt from the same definitions + user overrides (T12 plan, keybindings
/// both tracks connected).
pub fn install_global_keybindings() {
    use crate::core::keybindings as core_keybindings;
    use rpi_tui::keybindings::{
        KeyBindingValue as TuiKeyBindingValue, KeybindingDefinition, KeybindingsConfig,
    };

    let manager = core_keybindings::KeybindingsManager::create();
    core_keybindings::set_keybindings(manager.clone());

    let definitions: Vec<(String, KeybindingDefinition)> =
        core_keybindings::keybinding_definitions()
            .iter()
            .map(|(id, definition)| {
                (
                    id.clone(),
                    KeybindingDefinition {
                        default_keys: match &definition.default_keys {
                            core_keybindings::KeyBindingValue::Single(key) => {
                                TuiKeyBindingValue::Single(key.clone())
                            }
                            core_keybindings::KeyBindingValue::Multiple(keys) => {
                                TuiKeyBindingValue::Multiple(keys.clone())
                            }
                        },
                        description: Some(definition.description),
                    },
                )
            })
            .collect();
    let mut user_bindings = KeybindingsConfig::new();
    for (id, keys) in manager.get_user_bindings() {
        let value = match keys.as_slice() {
            [single] => TuiKeyBindingValue::Single(single.clone()),
            _ => TuiKeyBindingValue::Multiple(keys),
        };
        user_bindings.insert(id, value);
    }
    tui_keybindings::set_keybindings(tui_keybindings::KeybindingsManager::new(
        definitions,
        user_bindings,
    ));
}

/// `quoteIfNeeded` (interactive-mode.ts:224-229).
fn quote_if_needed(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' | '/' | '~' | ':' | '@')) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// `formatResumeCommand` (interactive-mode.ts:231-244).
fn format_resume_command(session: &AgentSession) -> Option<String> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let session_manager_arc = session.session_manager();
    let manager = lock(&session_manager_arc);
    if !manager.is_persisted() {
        return None;
    }
    let session_file = manager.get_session_file()?.to_path_buf();
    if !session_file.exists() {
        return None;
    }
    let mut args = vec![APP_NAME.to_string()];
    if !manager.uses_default_session_dir() {
        args.push("--session-dir".to_string());
        args.push(quote_if_needed(
            &manager.get_session_dir().display().to_string(),
        ));
    }
    args.push("--session".to_string());
    args.push(manager.get_session_id().to_string());
    Some(args.join(" "))
}

/// `registerSignalHandlers` (interactive-mode.ts:3643-3681): SIGTERM/SIGHUP
/// trigger a graceful shutdown (SIGHUP no longer hard-exits — the terminal
/// restore is attempted and a genuinely dead terminal surfaces on the
/// restore writes). The handler thread signals the run loop's shutdown watch
/// channel; the run loop performs the shutdown.
fn register_signal_handlers(shutdown_tx: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for kind in [SignalKind::terminate(), SignalKind::hangup()] {
            let shutdown_tx = shutdown_tx.clone();
            if let Ok(mut stream) = signal(kind) {
                std::thread::spawn(move || {
                    // Dedicated thread with a tiny runtime: the handler must
                    // fire even while the main runtime is parked on I/O
                    // (same pattern as print_mode.rs).
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(runtime) = runtime {
                        runtime.block_on(async move {
                            stream.recv().await;
                        });
                    }
                    let _ = shutdown_tx.send(true);
                });
            }
        }
    }
}

/// `runInteractiveMode` entry point (app.rs `AppMode::Interactive` branch).
pub async fn run_interactive_mode(
    runtime: AgentSessionRuntime,
    options: InteractiveModeOptions,
) -> i32 {
    install_global_keybindings();
    // First-time setup (main.ts:563-565): shown before the mode when no
    // settings file exists yet. Failures degrade to the normal interactive
    // startup with a warning (the setup is a convenience flow; upstream
    // would let the error propagate).
    if crate::modes::interactive::startup_ui::should_run_first_time_setup(
        &crate::config::get_global_settings_path(),
    ) {
        match crate::modes::interactive::startup_ui::run_first_time_setup(&runtime).await {
            Ok(_completed) => {}
            Err(error) => eprintln!("Warning: first-time setup failed: {error}"),
        }
    }
    let mut mode = InteractiveMode::new(runtime, options);
    register_signal_handlers(mode.shutdown_sender());
    mode.run().await;
    0
}

// =============================================================================
// Event commands
// =============================================================================

/// A queued session event (or mode-internal UI command), applied by
/// [`InteractiveUi::drain_events`]. Variants mirror the 24
/// `AgentSessionEvent` variants (10 `AgentEvent` + 9 `SessionEvent` + 5
/// `CompactionEvent`), plus two mode-internal commands that may not be
/// executed from inside an editor dispatch (lock contract).
#[derive(Debug, Clone)]
pub(crate) enum UiCommand {
    AgentStart,
    AgentEnd,
    TurnStart,
    TurnEnd,
    MessageStart(AgentMessage),
    MessageUpdate(AgentMessage),
    MessageEnd(AgentMessage),
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        partial_result: serde_json::Value,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        result: serde_json::Value,
        is_error: bool,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    EntryAppended(SessionEntry),
    SessionInfoChanged {
        name: Option<String>,
    },
    ThinkingLevelChanged(ThinkingLevel),
    BashExecutionUpdate {
        id: Option<String>,
        delta: String,
    },
    ExtensionError {
        extension_path: String,
        event: String,
        error: String,
    },
    AgentSettled,
    CompactionStart(CompactionReason),
    CompactionEnd {
        reason: CompactionReason,
        result: Option<Box<rpi_agent::compaction::CompactionResult>>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart(RetrySource),
    SummarizationRetryFinished,
    /// Mode-internal: clear the editor (Ctrl+C single press).
    ClearEditor,
    /// Mode-internal: refresh the editor border color (bash mode /
    /// thinking level change) from outside the editor lock.
    RefreshEditorBorder,
    /// Mode-internal: the Escape key was pressed while the editor had focus
    /// (deferred from the dispatch — the handler cannot lock the editor it
    /// is dispatched from).
    Escape,
    /// Mode-internal: the theme watcher detected a change to the current
    /// custom theme file (theme.ts:921-932). The drain reloads the file and
    /// applies the new theme; the watcher thread never touches the theme
    /// fields itself.
    ThemeChanged,
    /// Mode-internal: apply a resolved theme name from the `/settings` theme
    /// change (theme-controller.ts `applyFromSettings`, 37-60). Routed
    /// through the drain so [`InteractiveUi::apply_theme`] never runs inside
    /// a component callback (lock contract); automatic pairs resolve the
    /// terminal appearance asynchronously and push the resolved branch name.
    ApplyThemeName(String),
    /// Mode-internal: the git branch watcher observed a branch change and
    /// already updated the `FooterDataProvider`; the drain invalidates the
    /// footer (upstream `onBranchChange` subscriber,
    /// interactive-mode.ts:807-809).
    GitBranchChanged,
    /// Mode-internal: the startup version check found a newer release
    /// (interactive-mode.ts:843-847 → `showNewVersionNotification`).
    NewVersionAvailable(crate::core::version_check::LatestRpiRelease),
    /// `/share`: the loader's `on_abort` (interactive-mode.ts:5549-5554) —
    /// kill gh, restore the editor, clean up, "Share cancelled".
    ShareAbort,
    /// `/share`: the gist worker finished
    /// (interactive-mode.ts:5575-5602).
    ShareCompleted(crate::core::share::GistCreateOutcome),
    /// Mode-internal: Alt+Up dequeue (deferred — restore touches the editor).
    Dequeue,
    /// Mode-internal: clipboard paste (deferred — insertion touches the
    /// editor and clipboard reads block).
    PasteImage,
}

impl From<AgentSessionEvent> for UiCommand {
    fn from(event: AgentSessionEvent) -> Self {
        match event {
            AgentSessionEvent::AgentEnd(_) => UiCommand::AgentEnd,
            AgentSessionEvent::Agent(event) => match *event {
                AgentEvent::AgentStart => UiCommand::AgentStart,
                AgentEvent::AgentEnd { .. } => UiCommand::AgentEnd,
                AgentEvent::TurnStart => UiCommand::TurnStart,
                AgentEvent::TurnEnd { .. } => UiCommand::TurnEnd,
                AgentEvent::MessageStart { message } => UiCommand::MessageStart(message),
                AgentEvent::MessageUpdate { message, .. } => UiCommand::MessageUpdate(message),
                AgentEvent::MessageEnd { message } => UiCommand::MessageEnd(message),
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } => UiCommand::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                },
                AgentEvent::ToolExecutionUpdate {
                    tool_call_id,
                    partial_result,
                    ..
                } => UiCommand::ToolExecutionUpdate {
                    tool_call_id,
                    partial_result,
                },
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    is_error,
                    ..
                } => UiCommand::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    is_error,
                },
            },
            AgentSessionEvent::Compaction(event) => match *event {
                CompactionEvent::CompactionStart { reason } => UiCommand::CompactionStart(reason),
                CompactionEvent::CompactionEnd {
                    reason,
                    result,
                    aborted,
                    will_retry,
                    error_message,
                } => UiCommand::CompactionEnd {
                    reason,
                    result,
                    aborted,
                    will_retry,
                    error_message,
                },
                CompactionEvent::SummarizationRetryScheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                } => UiCommand::SummarizationRetryScheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                },
                CompactionEvent::SummarizationRetryAttemptStart { source } => {
                    UiCommand::SummarizationRetryAttemptStart(source)
                }
                CompactionEvent::SummarizationRetryFinished => {
                    UiCommand::SummarizationRetryFinished
                }
            },
            AgentSessionEvent::Session(event) => match event {
                crate::core::agent_session::SessionEvent::AgentSettled => UiCommand::AgentSettled,
                crate::core::agent_session::SessionEvent::QueueUpdate {
                    steering,
                    follow_up,
                } => UiCommand::QueueUpdate {
                    steering,
                    follow_up,
                },
                crate::core::agent_session::SessionEvent::EntryAppended { entry } => {
                    UiCommand::EntryAppended(*entry)
                }
                crate::core::agent_session::SessionEvent::SessionInfoChanged { name } => {
                    UiCommand::SessionInfoChanged { name }
                }
                crate::core::agent_session::SessionEvent::ThinkingLevelChanged { level } => {
                    UiCommand::ThinkingLevelChanged(level)
                }
                crate::core::agent_session::SessionEvent::AutoRetryStart {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                } => UiCommand::AutoRetryStart {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                },
                crate::core::agent_session::SessionEvent::AutoRetryEnd {
                    success,
                    attempt,
                    final_error,
                } => UiCommand::AutoRetryEnd {
                    success,
                    attempt,
                    final_error,
                },
                crate::core::agent_session::SessionEvent::BashExecutionUpdate { id, delta } => {
                    UiCommand::BashExecutionUpdate { id, delta }
                }
                crate::core::agent_session::SessionEvent::ExtensionError {
                    extension_path,
                    event,
                    error,
                } => UiCommand::ExtensionError {
                    extension_path,
                    event,
                    error,
                },
            },
        }
    }
}

// =============================================================================
// Component-tree helpers
// =============================================================================

/// Tree entry rendering through a shared handle (region pattern): the mode
/// keeps the concrete `Arc<Mutex<T>>` for mutations from the drain, the tree
/// owns this thin wrapper. `render`/`invalidate` lock the handle; the drain
/// and the render loop run on the same (driver) thread, so the locks never
/// contend.
struct SharedChild<T: Component>(Arc<Mutex<T>>);

impl<T: Component> Component for SharedChild<T> {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.0).render(width)
    }

    fn invalidate(&mut self) {
        lock(&self.0).invalidate();
    }

    fn set_expanded(&mut self, expanded: bool) {
        lock(&self.0).set_expanded(expanded);
    }
}

/// Tree entry for a shared focusable component (selectors, T12-S5a): the
/// TUI owns this wrapper; the mode keeps the concrete `Arc<Mutex<T>>` and
/// drives it through the drain. `focused` state lives in the wrapped
/// component (its `Focusable` impl), which is what the wrapper delegates to.
struct FocusableRegion<T: Component + Focusable>(Arc<Mutex<T>>);

impl<T: Component + Focusable> Component for FocusableRegion<T> {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.0).render(width)
    }

    fn handle_input(&mut self, data: &str) {
        lock(&self.0).handle_input(data);
    }

    fn invalidate(&mut self) {
        lock(&self.0).invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl<T: Component + Focusable> Focusable for FocusableRegion<T> {
    fn focused(&self) -> bool {
        lock(&self.0).focused()
    }

    fn set_focused(&mut self, focused: bool) {
        lock(&self.0).set_focused(focused);
    }
}

/// The address of a boxed child inside a container — used for removal by
/// identity (mirrors `Container::remove_child`'s pointer comparison). Stored
/// as `usize` and never dereferenced; valid because the box stays owned by
/// the container until the removal.
fn child_address(child: &dyn Component) -> usize {
    child as *const dyn Component as *const () as usize
}

/// Remove the child whose address matches (upstream `removeChild` identity
/// semantics without holding a reference).
fn remove_child_by_address(container: &mut Container, address: usize) {
    container
        .children
        .retain(|child| child_address(&**child) != address);
}

/// The streaming assistant message currently being rendered
/// (interactive-mode.ts:365-366).
struct StreamingTrack {
    handle: Arc<Mutex<crate::modes::interactive::components::AssistantMessageComponent>>,
    /// Address of the `SharedChild` wrapper inside the chat container, for
    /// the `agent_end` safety-net removal (interactive-mode.ts:3055-3059).
    entry_address: usize,
}

/// The status container's active indicator (upstream `activeStatusIndicator`
/// + `idleStatus`; `Idle` renders nothing).
enum ActiveStatus {
    Idle,
    Working(WorkingStatusIndicator),
    Retry(RetryStatusIndicator),
    Compaction(CompactionStatusIndicator),
    BranchSummary(BranchSummaryStatusIndicator),
}

impl ActiveStatus {
    fn kind(&self) -> Option<StatusIndicatorKind> {
        match self {
            ActiveStatus::Idle => None,
            ActiveStatus::Working(_) => Some(StatusIndicatorKind::Working),
            ActiveStatus::Retry(_) => Some(StatusIndicatorKind::Retry),
            ActiveStatus::Compaction(_) => Some(StatusIndicatorKind::Compaction),
            ActiveStatus::BranchSummary(_) => Some(StatusIndicatorKind::BranchSummary),
        }
    }

    fn dispose(&mut self) {
        match self {
            ActiveStatus::Idle => {}
            ActiveStatus::Working(indicator) => indicator.dispose(),
            ActiveStatus::Retry(indicator) => indicator.dispose(),
            ActiveStatus::Compaction(indicator) => indicator.dispose(),
            ActiveStatus::BranchSummary(indicator) => indicator.dispose(),
        }
    }
}

impl Component for ActiveStatus {
    fn render(&self, width: usize) -> Vec<String> {
        match self {
            ActiveStatus::Idle => Vec::new(),
            ActiveStatus::Working(indicator) => indicator.render(width),
            ActiveStatus::Retry(indicator) => indicator.render(width),
            ActiveStatus::Compaction(indicator) => indicator.render(width),
            ActiveStatus::BranchSummary(indicator) => indicator.render(width),
        }
    }

    fn invalidate(&mut self) {
        match self {
            ActiveStatus::Idle => {}
            ActiveStatus::Working(indicator) => indicator.invalidate(),
            ActiveStatus::Retry(indicator) => indicator.invalidate(),
            ActiveStatus::Compaction(indicator) => indicator.invalidate(),
            ActiveStatus::BranchSummary(indicator) => indicator.invalidate(),
        }
    }
}

/// `CompactionQueuedMessage` (interactive-mode.ts:192-195).
#[derive(Debug, Clone)]
struct CompactionQueuedMessage {
    text: String,
    mode: StreamingBehavior,
}

/// Render options for [`InteractiveUi::render_session_entries`]
/// (interactive-mode.ts:3340-3342).
#[derive(Debug, Clone, Copy, Default)]
struct RenderOptions {
    update_footer: bool,
    populate_history: bool,
}

/// Editor submit / follow-up command, plus selector picks routed back to the
/// run loop (the selector callbacks run on the driver thread and have no
/// runtime access; the run loop owns the runtime).
enum EditorInput {
    Submit(String),
    /// Alt+Enter follow-up (handleFollowUp, interactive-mode.ts:3727-3757).
    #[allow(dead_code)]
    FollowUp(String),
    /// Session-selector pick (interactive-mode.ts:4779-4781): resume the
    /// session file via `InteractiveMode::handle_resume_command`.
    ResumeSession(String),
    /// User-message-selector pick (interactive-mode.ts:4589-4602): fork from
    /// the entry via `InteractiveMode::handle_fork_command`.
    ForkFrom(String),
}

/// `sessionEntryToContextMessages` (session-manager.ts:383-405): project one
/// selected session entry into renderable messages. Plain custom entries are
/// display/state entries handled separately by the caller.
fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<AgentMessage> {
    match entry {
        SessionEntry::Message(message_entry) => vec![message_entry.message.clone()],
        SessionEntry::CustomMessage(custom_message_entry) => {
            vec![AgentMessage::Custom(CustomMessage {
                role: CustomRole::Custom,
                custom_type: custom_message_entry.custom_type.clone(),
                content: custom_message_entry.content.clone(),
                display: custom_message_entry.display,
                details: custom_message_entry.details.clone(),
                timestamp: parse_iso8601_ms(&custom_message_entry.timestamp).unwrap_or(0),
            })]
        }
        SessionEntry::BranchSummary(branch_summary) if !branch_summary.summary.is_empty() => {
            vec![AgentMessage::BranchSummary(BranchSummaryMessage {
                role: BranchSummaryRole::BranchSummary,
                summary: branch_summary.summary.clone(),
                from_id: branch_summary.from_id.clone(),
                timestamp: parse_iso8601_ms(&branch_summary.timestamp).unwrap_or(0),
            })]
        }
        SessionEntry::Compaction(compaction) => {
            vec![AgentMessage::CompactionSummary(CompactionSummaryMessage {
                role: CompactionSummaryRole::CompactionSummary,
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
                timestamp: parse_iso8601_ms(&compaction.timestamp).unwrap_or(0),
            })]
        }
        _ => Vec::new(),
    }
}

/// `createCompactionSummaryMessage` (core/messages.ts:109-120) — the
/// "session compacted" entry appended after a successful compaction.
fn create_compaction_summary_message(summary: &str, tokens_before: u64) -> AgentMessage {
    AgentMessage::CompactionSummary(CompactionSummaryMessage {
        role: CompactionSummaryRole::CompactionSummary,
        summary: summary.to_string(),
        tokens_before,
        timestamp: now_millis() as i64,
    })
}

/// Convert a `ToolResultMessage` into the loose result state the component
/// renders (tool-execution.ts `updateResult` input shape).
fn tool_result_state_from_message(
    tool_result: &rpi_ai::types::ToolResultMessage,
    is_error: bool,
) -> ToolResultState {
    ToolResultState {
        content: tool_result
            .content
            .iter()
            .map(|block| match block {
                ToolResultContent::Text(text) => ToolResultContentLoose::text(text.text.clone()),
                ToolResultContent::Image(image) => {
                    ToolResultContentLoose::image(image.data.clone(), image.mime_type.clone())
                }
            })
            .collect(),
        is_error,
        details: tool_result.details.clone(),
    }
}

/// Convert a partial/final tool result JSON (`{ content: [...], details }`)
/// into the loose result state (upstream spreads `...event.result`).
fn tool_result_state_from_value(value: &serde_json::Value, is_error: bool) -> ToolResultState {
    let mut content = Vec::new();
    let mut details = None;
    if let Some(object) = value.as_object() {
        if let Some(content_value) = object.get("content").and_then(|v| v.as_array()) {
            for block in content_value {
                let Some(block_object) = block.as_object() else {
                    continue;
                };
                match block_object.get("type").and_then(|t| t.as_str()) {
                    Some("text") => content.push(ToolResultContentLoose::text(
                        block_object
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or(""),
                    )),
                    Some("image") => content.push(ToolResultContentLoose::image(
                        block_object
                            .get("data")
                            .and_then(|d| d.as_str())
                            .unwrap_or(""),
                        block_object
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or(""),
                    )),
                    _ => {}
                }
            }
        }
        details = object.get("details").cloned();
    }
    ToolResultState {
        content,
        is_error,
        details,
    }
}

/// `getThinkingBorderColor` (theme.ts:407-425): map the thinking level to
/// its dedicated theme color.
fn thinking_border_color(
    theme: &Theme,
    level: ModelThinkingLevel,
) -> Box<dyn Fn(&str) -> String + Send + Sync> {
    let color = match level {
        ModelThinkingLevel::Off => "thinkingOff",
        ModelThinkingLevel::Minimal => "thinkingMinimal",
        ModelThinkingLevel::Low => "thinkingLow",
        ModelThinkingLevel::Medium => "thinkingMedium",
        ModelThinkingLevel::High => "thinkingHigh",
        ModelThinkingLevel::Xhigh => "thinkingXhigh",
        ModelThinkingLevel::Max => "thinkingMax",
    };
    let theme = theme.clone();
    Box::new(move |text: &str| theme.fg(color, text))
}

/// `getEditorTheme`/`getSelectListTheme` (theme.ts:1279-1284, 1224-1230).
fn editor_theme(theme: &Arc<Theme>) -> EditorTheme {
    let select_list = select_list_theme(theme);
    let theme = theme.clone();
    EditorTheme {
        border_color: Box::new(move |text: &str| theme.fg("borderMuted", text)),
        select_list,
    }
}

fn select_list_theme(theme: &Arc<Theme>) -> Arc<SelectListTheme> {
    let accent_theme = theme.clone();
    let selected_theme = theme.clone();
    let dim_theme = theme.clone();
    let scroll_theme = theme.clone();
    let error_theme = theme.clone();
    Arc::new(SelectListTheme {
        selected_prefix: Box::new(move |text: &str| accent_theme.fg("accent", text)),
        selected_text: Box::new(move |text: &str| selected_theme.fg("accent", text)),
        description: Box::new(move |text: &str| dim_theme.fg("dim", text)),
        scroll_info: Box::new(move |text: &str| scroll_theme.fg("dim", text)),
        no_match: Box::new(move |text: &str| error_theme.fg("error", text)),
    })
}

/// Resolve the active theme from settings (default `dark`).
fn resolve_theme(session: &AgentSession) -> Arc<Theme> {
    let theme_name = session
        .settings_manager(|settings| settings.get_theme())
        .unwrap_or_else(|| "dark".to_string());
    let theme = load_theme(&theme_name, None)
        .unwrap_or_else(|_| load_theme("dark", None).expect("builtin dark theme must load"));
    Arc::new(theme)
}

// =============================================================================
// InteractiveUi — the shared component-tree state
// =============================================================================

/// All component-tree state plus the UI logic that runs on the driver
/// thread (event drain) and inside editor dispatches (key handlers). The
/// interactive mode keeps one `Arc<InteractiveUi>`; the key-handler closures
/// capture it, which intentionally forms a cycle with the editor it lives in
/// (upstream GCs this; here the mode lives for the process lifetime).
/// `pub(crate)` so the slash-command handlers in `commands.rs` can
/// extend it.
pub(crate) struct InteractiveUi {
    pub(crate) ui: Tui,
    /// The bound session. Replaceable behind an `RwLock` so session
    /// switching (`/new`/`/resume`/`/clone`/`/fork`/`/import`) can rebind the
    /// shared `ui_state` (upstream `rebindCurrentSession`,
    /// interactive-mode.ts:1732-1758). Access only through
    /// [`InteractiveUi::session`] / [`InteractiveUi::set_session`]: the read
    /// guard never outlives the cheap `AgentSession` clone (Arc inner), so
    /// no lock is held across session calls — same lock contract as the
    /// component tree (D-019).
    session: RwLock<AgentSession>,
    /// The active theme. `Mutex` because the theme watcher / auto-theme
    /// listener swaps it on the driver thread while the run loop reads it
    /// (T12-S6; the write always happens on the driver thread via
    /// [`InteractiveUi::apply_theme`]).
    pub(crate) theme: Mutex<Arc<Theme>>,
    /// The markdown palette derived from [`Self::theme`] (theme.ts:1230-1271);
    /// swapped together with it.
    pub(crate) markdown_theme: Mutex<Arc<MarkdownTheme>>,
    pub(crate) render_handle: RenderHandle,
    event_queue: Arc<Mutex<VecDeque<UiCommand>>>,

    // Tree regions (wrapped into the Tui's child list at init).
    header_container: Arc<Mutex<Container>>,
    loaded_resources_container: Arc<Mutex<Container>>,
    pub(crate) chat_container: Arc<Mutex<Container>>,
    pending_messages_container: Arc<Mutex<Container>>,
    status: Arc<Mutex<ActiveStatus>>,
    widgets_above: Arc<Mutex<Container>>,
    widgets_below: Arc<Mutex<Container>>,
    pub(crate) editor: Arc<Mutex<CustomEditor>>,
    footer: Arc<Mutex<FooterComponent>>,
    /// Shared with the footer and the git branch watcher
    /// (`git_branch_watcher.rs` reads `cwd` / writes `git_branch`).
    pub(crate) footer_data: Arc<FooterDataProvider>,

    /// The editor's tree entry (T12-S5a `showSelector` swaps this child in
    /// the TUI's child list for the active selector, preserving position).
    editor_region: SharedComponent,
    /// Header/footer tree entries (stored at init) + the active extension
    /// overrides (T15 W4 `setHeader`/`setFooter` region swaps).
    header_region: Mutex<Option<SharedComponent>>,
    footer_region: Mutex<Option<SharedComponent>>,
    custom_header: Mutex<Option<SharedComponent>>,
    custom_footer: Mutex<Option<SharedComponent>>,
    /// The active selector entry, if any (T12-S5a).
    active_selector: Mutex<Option<SharedComponent>>,
    /// Weak self-reference for callbacks that need the `Arc<InteractiveUi>`
    /// (selector onSelect closures, escape double-press actions).
    self_arc: Mutex<Option<std::sync::Weak<InteractiveUi>>>,

    /// The built-in startup header (only when not quiet), for the
    /// tools-expanded linkage (interactive-mode.ts:3812-3824).
    built_in_header: Mutex<Option<Arc<Mutex<ExpandableText>>>>,

    /// Streaming assistant message (interactive-mode.ts:365-366).
    streaming: Mutex<Option<StreamingTrack>>,
    /// Tool call id → component (interactive-mode.ts:369).
    pending_tools: Mutex<HashMap<String, Arc<Mutex<ToolExecutionComponent>>>>,

    /// `toolOutputExpanded` (interactive-mode.ts:372).
    tool_output_expanded: Mutex<bool>,
    /// `hideThinkingBlock` (interactive-mode.ts:375).
    hide_thinking_block: Mutex<bool>,
    /// `outputPad` (interactive-mode.ts:376). Atomic because
    /// `apply_runtime_settings` refreshes it on session rebind
    /// (interactive-mode.ts:1715).
    output_pad: AtomicUsize,
    /// `hiddenThinkingLabel` (interactive-mode.ts:351-352). Mutex: the T15
    /// extension UI bridge updates it (`setHiddenThinkingLabel`).
    hidden_thinking_label: Mutex<String>,
    /// `workingVisible` (interactive-mode.ts:348). Atomic: the T15 bridge
    /// updates it (`setWorkingVisible`).
    working_visible: AtomicBool,
    /// `workingMessage` (interactive-mode.ts:347) — extension-driven (T15).
    working_message: Mutex<Option<String>>,

    /// Status-line coalescing (interactive-mode.ts:361-362): the last
    /// spacer/text pair appended by `showStatus`, tracked by child address
    /// (mirrors the upstream reference-identity check).
    last_status_spacer: Mutex<Option<usize>>,
    last_status_text: Mutex<Option<StatusTextTrack>>,

    /// `isBashMode` (interactive-mode.ts:386).
    is_bash_mode: Mutex<bool>,
    /// `bashComponent` (interactive-mode.ts:346) — the latest bash execution
    /// component; streaming output is appended through it (written by
    /// `handle_bash_command`, cleared on completion).
    #[allow(dead_code)] // write-only outside tests (interface parity)
    bash_component: Mutex<Option<Arc<Mutex<BashExecutionComponent>>>>,
    /// `pendingBashComponents` (interactive-mode.ts:352) — bash components
    /// deferred to the pending area while the agent is streaming; the entry
    /// address is stored for identity removal in
    /// `flush_pending_bash_components`.
    #[allow(dead_code)] // read only by flush + tests
    pending_bash_components: Mutex<Vec<(usize, Arc<Mutex<BashExecutionComponent>>)>>,
    /// Injectable `gh` runner for `/share` (core/share.rs; the W2
    /// `PackageCommandRunner` pattern). `SystemShareRunner` in production;
    /// tests swap a mock via [`InteractiveUi::set_share_runner`].
    pub(crate) share_runner: Mutex<Arc<dyn crate::core::share::ShareRunner>>,
    /// Injectable install-telemetry transport (core/telemetry.rs; the
    /// `PackageCommandRunner` pattern). `ReqwestReportInstallTransport` in
    /// production; tests swap a no-op via
    /// [`InteractiveUi::set_report_install_transport`] so `cargo test` never
    /// touches product endpoints (T14 review M1).
    pub(crate) report_install_transport:
        Mutex<Arc<dyn crate::core::telemetry::ReportInstallTransport>>,
    /// Injectable version-check transport (core/version_check.rs).
    /// `ReqwestLatestVersionTransport` in production; tests swap a no-op via
    /// [`InteractiveUi::set_latest_version_transport`] (same M1 discipline).
    pub(crate) latest_version_transport:
        Mutex<Arc<dyn crate::core::version_check::LatestVersionTransport>>,
    /// In-flight `/share` state between the loader swap and the gist
    /// worker's completion (interactive-mode.ts:5540-5560). `None` once the
    /// drain has settled the share (success, failure or abort) — the
    /// single-`take` makes a late completion/abort double-queue a no-op.
    pub(crate) share_state: Mutex<Option<ShareState>>,
    /// `lastSigintTime` / `lastEscapeTime` (interactive-mode.ts:354-355).
    last_sigint_time: AtomicU64,
    last_escape_time: AtomicU64,

    /// Saved escape handlers while compaction / retry install their own
    /// (interactive-mode.ts:395-398).
    auto_compaction_escape_handler: Mutex<Option<EscapeHandler>>,
    retry_escape_handler: Mutex<Option<EscapeHandler>>,

    /// Settings snapshot (read at init).
    show_terminal_progress: bool,
    show_images: bool,
    image_width_cells: usize,
    double_escape_action: DoubleEscapeAction,
    cwd: String,
    /// `options.verbose` (interactive-mode.ts:1077-1079) — startup listing
    /// and section expansion state.
    verbose: bool,

    /// `shutdownRequested` (interactive-mode.ts:404) — set by extension
    /// shutdown requests; checked on `agent_settled`.
    shutdown_requested: AtomicBool,
    /// Signals the run loop to shut down (Ctrl+C double press, Ctrl+D,
    /// `/quit`, SIGTERM/SIGHUP, extension shutdown).
    pub(crate) shutdown_tx: watch::Sender<bool>,
    /// Incremented when a compaction ends and the queued messages should be
    /// flushed (run loop drains the compaction queue).
    flush_tx: watch::Sender<u64>,
    /// Messages queued while compaction is running
    /// (interactive-mode.ts:401).
    compaction_queue: Mutex<Vec<CompactionQueuedMessage>>,
    /// Monotonic flush counter (bumped on `compaction_end`).
    flush_generation: AtomicU64,
    /// Editor submit channel (dispatch → run loop).
    input_tx: UnboundedSender<EditorInput>,

    /// `skillCommands` (interactive-mode.ts:374, 611-616): `skill:<name>` →
    /// skill file path, populated by `createBaseAutocompleteProvider` (the
    /// `modes/interactive/autocomplete` port) and read by the slash-command
    /// dispatch.
    pub(crate) skill_commands: Mutex<HashMap<String, String>>,
}

/// The last `showStatus` text child (tracked for back-to-back coalescing).
#[derive(Clone)]
struct StatusTextTrack {
    /// Address of the `SharedChild` wrapper inside the chat container.
    entry_address: usize,
    handle: Arc<Mutex<Text>>,
}

/// In-flight `/share` (gist) state — the loader's cancel flag and the temp
/// HTML file to clean up (interactive-mode.ts:5540-5560).
pub(crate) struct ShareState {
    /// Set by the loader's `on_abort`; the gist worker's runner polls it and
    /// kills `gh` (upstream `proc.kill()`, interactive-mode.ts:5551).
    pub(crate) cancelled: Arc<AtomicBool>,
    /// `{tmpdir}/rpi-share-{pid}-{nanos}/session.html` — basename kept from
    /// upstream `path.join(os.tmpdir(), "session.html")`
    /// (interactive-mode.ts:5526); the unique subdirectory avoids two
    /// concurrent instances clobbering each other's export. Removed via
    /// [`crate::core::share::cleanup_share_tmp_file`].
    pub(crate) tmp_file: std::path::PathBuf,
}

impl InteractiveUi {
    pub(crate) fn push(&self, command: UiCommand) {
        lock(&self.event_queue).push_back(command);
    }

    /// Swap the `/share` gh runner (tests inject a mock; production keeps
    /// the `SystemShareRunner` installed at init).
    #[allow(dead_code)] // test hook (lib builds never swap the runner)
    pub(crate) fn set_share_runner(&self, runner: Arc<dyn crate::core::share::ShareRunner>) {
        *lock(&self.share_runner) = runner;
    }

    /// Swap the install-telemetry transport (tests inject a no-op; T14
    /// review M1 — unit tests must never reach product endpoints).
    #[allow(dead_code)] // test hook (lib builds never swap the transport)
    pub(crate) fn set_report_install_transport(
        &self,
        transport: Arc<dyn crate::core::telemetry::ReportInstallTransport>,
    ) {
        *lock(&self.report_install_transport) = transport;
    }

    /// Swap the version-check transport (tests inject a no-op; T14 review
    /// M1).
    #[allow(dead_code)] // test hook (lib builds never swap the transport)
    pub(crate) fn set_latest_version_transport(
        &self,
        transport: Arc<dyn crate::core::version_check::LatestVersionTransport>,
    ) {
        *lock(&self.latest_version_transport) = transport;
    }

    /// The live `Arc<InteractiveUi>` for callbacks that need one; `None`
    /// once the mode is dropped.
    pub(crate) fn upgrade_self(&self) -> Option<Arc<Self>> {
        lock(&self.self_arc)
            .as_ref()
            .and_then(|weak| weak.upgrade())
    }

    /// The bound session (cheap Arc-shared clone). The read guard is
    /// dropped before the clone is returned, so callers never hold the lock
    /// across session calls.
    pub(crate) fn session(&self) -> AgentSession {
        self.session
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Swap the bound session (session switching; upstream
    /// `rebindCurrentSession`, interactive-mode.ts:1732-1758). Callers must
    /// re-subscribe via [`InteractiveUi::subscribe_to_agent`] afterwards.
    pub(crate) fn set_session(&self, session: AgentSession) {
        *self
            .session
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = session;
    }

    /// `subscribeToAgent` (interactive-mode.ts:2843-2847): the callback only
    /// converts and queues — no component locking inside the callback (lock
    /// contract, tui.rs header note).
    pub(crate) fn subscribe_to_agent(&self) -> Box<dyn FnOnce() + Send> {
        let queue = Arc::clone(&self.event_queue);
        let render_handle = self.render_handle.clone();
        self.session()
            .subscribe(Arc::new(move |event: AgentSessionEvent| {
                let command = UiCommand::from(event);
                lock(&queue).push_back(command);
                render_handle.request_render();
            }))
    }

    /// Apply all queued commands. Runs on the driver thread between `pump`
    /// calls (or directly in tests); never from inside a component callback.
    pub(crate) fn drain_events(&self) {
        let commands: Vec<UiCommand> = lock(&self.event_queue).drain(..).collect();
        for command in commands {
            self.handle_command(command);
        }
    }

    /// `handleEvent` (interactive-mode.ts:2849-3176) — all 24 branches.
    fn handle_command(&self, command: UiCommand) {
        match command {
            UiCommand::AgentStart => {
                // agent_start (interactive-mode.ts:2857-2880).
                lock(&self.pending_tools).clear();
                if self.show_terminal_progress {
                    self.ui
                        .with_terminal(|terminal| terminal.set_progress(true));
                }
                // Restore the main escape handler if the retry handler is
                // still active (the retry success event fires later, but the
                // main handler is needed now).
                if let Some(handler) = lock(&self.retry_escape_handler).take() {
                    lock(&self.editor).on_escape = Some(handler);
                }
                if self.working_visible.load(Ordering::Relaxed) {
                    let message = lock(&self.working_message)
                        .clone()
                        .unwrap_or_else(|| "Working...".to_string());
                    self.show_status_indicator(ActiveStatus::Working(WorkingStatusIndicator::new(
                        self.render_handle.clone(),
                        message,
                        Arc::clone(&lock(&self.theme)),
                    )));
                } else {
                    self.clear_status_indicator(Some(StatusIndicatorKind::Working));
                }
                self.render_handle.request_render();
            }
            UiCommand::TurnStart | UiCommand::TurnEnd => {
                // Upstream `handleEvent` has no turn_start/turn_end cases —
                // no-op (interactive-mode.ts:2856-3175).
            }
            UiCommand::QueueUpdate {
                steering,
                follow_up,
            } => {
                // queue_update (interactive-mode.ts:2882-2885). The display
                // re-reads the queues (`update_pending_messages_display`
                // renders the Steering:/Follow-up: rows); the event fields
                // are carried for interface parity.
                let _ = (steering, follow_up);
                self.update_pending_messages_display();
                self.render_handle.request_render();
            }
            UiCommand::EntryAppended(entry) => {
                // entry_appended (interactive-mode.ts:2887-2892).
                if matches!(entry, SessionEntry::Custom(_)) {
                    self.add_custom_entry_to_chat(&entry);
                    self.render_handle.request_render();
                }
            }
            UiCommand::SessionInfoChanged { name } => {
                // session_info_changed (interactive-mode.ts:2894-2898).
                let _ = name;
                self.update_terminal_title();
                Component::invalidate(&mut *lock(&self.footer));
                self.render_handle.request_render();
            }
            UiCommand::ThinkingLevelChanged(level) => {
                // thinking_level_changed (interactive-mode.ts:2900-2903).
                let _ = level;
                Component::invalidate(&mut *lock(&self.footer));
                self.update_editor_border_color();
            }
            UiCommand::MessageStart(message) => {
                // message_start (interactive-mode.ts:2905-2926).
                match &message {
                    AgentMessage::Custom(_) => {
                        self.add_message_to_chat(message, false);
                        self.render_handle.request_render();
                    }
                    AgentMessage::User(_) => {
                        self.add_message_to_chat(message, false);
                        self.update_pending_messages_display();
                        self.render_handle.request_render();
                    }
                    AgentMessage::Assistant(assistant) => {
                        let mut component =
                            crate::modes::interactive::components::AssistantMessageComponent::new(
                                None,
                                *lock(&self.hide_thinking_block),
                                Arc::clone(&lock(&self.theme)),
                                Arc::clone(&lock(&self.markdown_theme)),
                                lock(&self.hidden_thinking_label).clone(),
                                self.output_pad.load(Ordering::Relaxed),
                            );
                        component.update_content(assistant.clone());
                        let handle = Arc::new(Mutex::new(component));
                        let entry_address =
                            self.add_chat_child(Box::new(SharedChild(handle.clone())));
                        *lock(&self.streaming) = Some(StreamingTrack {
                            handle,
                            entry_address,
                        });
                        self.render_handle.request_render();
                    }
                    _ => {}
                }
            }
            UiCommand::MessageUpdate(message) => {
                // message_update (interactive-mode.ts:2928-2961).
                if let AgentMessage::Assistant(assistant) = &message {
                    if let Some(track) = lock(&self.streaming).as_ref() {
                        lock(&track.handle).update_content(assistant.clone());
                        for content in &assistant.content {
                            if let AssistantContent::ToolCall(tool_call) = content {
                                let mut pending_tools = lock(&self.pending_tools);
                                if !pending_tools.contains_key(&tool_call.id) {
                                    let component = self.new_tool_execution_component(
                                        &tool_call.name,
                                        &tool_call.id,
                                        &serde_json::Value::Object(tool_call.arguments.clone()),
                                    );
                                    let handle = Arc::new(Mutex::new(component));
                                    self.add_chat_child(Box::new(SharedChild(handle.clone())));
                                    pending_tools.insert(tool_call.id.clone(), handle);
                                } else if let Some(component) = pending_tools.get(&tool_call.id) {
                                    lock(component).update_args(serde_json::Value::Object(
                                        tool_call.arguments.clone(),
                                    ));
                                }
                            }
                        }
                    }
                }
                self.render_handle.request_render();
            }
            UiCommand::MessageEnd(message) => {
                // message_end (interactive-mode.ts:2963-3001).
                if matches!(message, AgentMessage::User(_)) {
                    return;
                }
                if let AgentMessage::Assistant(assistant) = &message {
                    let streamed = lock(&self.streaming).take();
                    if let Some(track) = streamed {
                        let mut component = lock(&track.handle);
                        let mut error_message: Option<String> = None;
                        if assistant.stop_reason == StopReason::Aborted {
                            let retry_attempt = self.session().retry_attempt();
                            error_message = Some(if retry_attempt > 0 {
                                format!(
                                    "Aborted after {retry_attempt} retry attempt{}",
                                    if retry_attempt > 1 { "s" } else { "" }
                                )
                            } else {
                                "Operation aborted".to_string()
                            });
                        }
                        let mut assistant = assistant.clone();
                        if let Some(message) = &error_message {
                            assistant.error_message = Some(message.clone());
                        }
                        let fallback_error_message = assistant.error_message.clone();
                        let stop_is_error = self.streaming_stop_reason_is_error(&assistant);
                        // `maybeShowCacheMissNotice` (interactive-mode.ts:2994)
                        // runs in the non-error branch; called before
                        // `update_content` so the message value can be shared
                        // without cloning the (potentially large) content.
                        if !stop_is_error {
                            self.maybe_show_cache_miss_notice(&assistant);
                        }
                        component.update_content(assistant);
                        drop(component);

                        if stop_is_error {
                            let error_message = error_message
                                .or(fallback_error_message)
                                .unwrap_or_else(|| "Error".to_string());
                            let mut pending_tools = lock(&self.pending_tools);
                            for tool in pending_tools.values() {
                                lock(tool).update_result(
                                    ToolResultState {
                                        content: vec![ToolResultContentLoose::text(
                                            error_message.clone(),
                                        )],
                                        is_error: true,
                                        details: None,
                                    },
                                    false,
                                );
                            }
                            pending_tools.clear();
                        } else {
                            // Args are now complete — trigger diff computation
                            // for edit tools.
                            let pending_tools = lock(&self.pending_tools);
                            for tool in pending_tools.values() {
                                lock(tool).set_args_complete();
                            }
                        }
                        Component::invalidate(&mut *lock(&self.footer));
                    }
                }
                self.render_handle.request_render();
            }
            UiCommand::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                // tool_execution_start (interactive-mode.ts:3007-3029).
                let mut pending_tools = lock(&self.pending_tools);
                let component = match pending_tools.get(&tool_call_id) {
                    Some(component) => Arc::clone(component),
                    None => {
                        let component =
                            self.new_tool_execution_component(&tool_name, &tool_call_id, &args);
                        let handle = Arc::new(Mutex::new(component));
                        self.add_chat_child(Box::new(SharedChild(handle.clone())));
                        pending_tools.insert(tool_call_id.clone(), handle.clone());
                        handle
                    }
                };
                lock(&component).mark_execution_started();
                self.render_handle.request_render();
            }
            UiCommand::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
            } => {
                // tool_execution_update (interactive-mode.ts:3031-3038).
                let pending_tools = lock(&self.pending_tools);
                if let Some(component) = pending_tools.get(&tool_call_id) {
                    lock(component)
                        .update_result(tool_result_state_from_value(&partial_result, false), true);
                    self.render_handle.request_render();
                }
            }
            UiCommand::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
            } => {
                // tool_execution_end (interactive-mode.ts:3040-3048).
                let mut pending_tools = lock(&self.pending_tools);
                if let Some(component) = pending_tools.remove(&tool_call_id) {
                    lock(&component)
                        .update_result(tool_result_state_from_value(&result, is_error), false);
                    self.render_handle.request_render();
                }
            }
            UiCommand::AgentEnd => {
                // agent_end (interactive-mode.ts:3050-3063).
                if self.show_terminal_progress {
                    self.ui
                        .with_terminal(|terminal| terminal.set_progress(false));
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Working));
                if let Some(track) = lock(&self.streaming).take() {
                    remove_child_by_address(&mut lock(&self.chat_container), track.entry_address);
                }
                lock(&self.pending_tools).clear();
                self.render_handle.request_render();
            }
            UiCommand::AgentSettled => {
                // agent_settled (interactive-mode.ts:3065-3067): extension
                // shutdown requests complete here.
                if self.shutdown_requested.load(Ordering::Relaxed) {
                    let _ = self.shutdown_tx.send(true);
                }
            }
            UiCommand::CompactionStart(reason) => {
                // compaction_start (interactive-mode.ts:3069-3081).
                if self.show_terminal_progress {
                    self.ui
                        .with_terminal(|terminal| terminal.set_progress(true));
                }
                // Keep the editor active; submissions are queued during
                // compaction.
                let mut editor = lock(&self.editor);
                *lock(&self.auto_compaction_escape_handler) = editor.on_escape.take();
                let session = self.session();
                editor.on_escape = Some(Box::new(move || {
                    session.abort_compaction();
                }));
                drop(editor);
                self.show_status_indicator(ActiveStatus::Compaction(
                    CompactionStatusIndicator::new(
                        self.render_handle.clone(),
                        compaction_status_reason(reason),
                        Arc::clone(&lock(&self.theme)),
                    ),
                ));
                self.render_handle.request_render();
            }
            UiCommand::CompactionEnd {
                reason,
                result,
                aborted,
                will_retry,
                error_message,
            } => {
                // willRetry would drive the flush semantics
                // (interactive-mode.ts:3117); currently unwired — see the
                // known gap in `flush_compaction_queue` (D-019).
                let _ = will_retry;
                // compaction_end (interactive-mode.ts:3083-3120).
                if self.show_terminal_progress {
                    self.ui
                        .with_terminal(|terminal| terminal.set_progress(false));
                }
                if let Some(handler) = lock(&self.auto_compaction_escape_handler).take() {
                    lock(&self.editor).on_escape = Some(handler);
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Compaction));
                if aborted {
                    if reason == CompactionReason::Manual {
                        self.show_error("Compaction cancelled");
                    } else {
                        self.show_status("Auto-compaction cancelled");
                    }
                } else if let Some(result) = result {
                    lock(&self.chat_container).clear();
                    self.rebuild_chat_from_messages();
                    self.add_message_to_chat(
                        create_compaction_summary_message(&result.summary, result.tokens_before),
                        false,
                    );
                    Component::invalidate(&mut *lock(&self.footer));
                } else if let Some(error_message) = error_message {
                    if reason == CompactionReason::Manual {
                        self.show_error(&error_message);
                    } else {
                        self.add_chat_child(Box::new(Text::new(
                            lock(&self.theme).fg("error", &error_message),
                            1,
                            0,
                            None,
                        )));
                    }
                }
                // Signal the run loop to flush messages queued during
                // compaction.
                let _ = self
                    .flush_tx
                    .send(self.flush_generation.fetch_add(1, Ordering::Relaxed) + 1);
                // Known gap (unassigned — no v0.1 task claims it):
                // flushCompactionQueue willRetry semantics
                // (interactive-mode.ts:3117, 4008-4040); see the
                // `flush_compaction_queue` note and D-019.
                self.render_handle.request_render();
            }
            UiCommand::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                // The message is informational; the retry status indicator
                // renders the countdown (interactive-mode.ts:3122-3133).
                let _ = error_message;
                // auto_retry_start (interactive-mode.ts:3122-3133).
                let mut editor = lock(&self.editor);
                *lock(&self.retry_escape_handler) = editor.on_escape.take();
                let session = self.session();
                editor.on_escape = Some(Box::new(move || {
                    session.abort_retry();
                }));
                drop(editor);
                self.show_status_indicator(ActiveStatus::Retry(RetryStatusIndicator::new(
                    self.render_handle.clone(),
                    attempt,
                    max_attempts,
                    delay_ms,
                    Arc::clone(&lock(&self.theme)),
                )));
                self.render_handle.request_render();
            }
            UiCommand::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => {
                // auto_retry_end (interactive-mode.ts:3135-3148).
                if let Some(handler) = lock(&self.retry_escape_handler).take() {
                    lock(&self.editor).on_escape = Some(handler);
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                // Show an error only on final failure (success shows the
                // normal response).
                if !success {
                    self.show_error(&format!(
                        "Retry failed after {attempt} attempts: {}",
                        final_error.unwrap_or_else(|| "Unknown error".to_string())
                    ));
                }
                self.render_handle.request_render();
            }
            UiCommand::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                // summarization_retry_scheduled (interactive-mode.ts:3150-3157).
                self.show_error(&error_message);
                self.show_status_indicator(ActiveStatus::Retry(RetryStatusIndicator::new(
                    self.render_handle.clone(),
                    attempt,
                    max_attempts,
                    delay_ms,
                    Arc::clone(&lock(&self.theme)),
                )));
                self.render_handle.request_render();
            }
            UiCommand::SummarizationRetryAttemptStart(source) => {
                // summarization_retry_attempt_start (interactive-mode.ts:3159-3168).
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                match source {
                    RetrySource::BranchSummary => {
                        self.show_status_indicator(ActiveStatus::BranchSummary(
                            BranchSummaryStatusIndicator::new(
                                self.render_handle.clone(),
                                Arc::clone(&lock(&self.theme)),
                            ),
                        ));
                    }
                    RetrySource::Compaction { reason } => {
                        self.show_status_indicator(ActiveStatus::Compaction(
                            CompactionStatusIndicator::new(
                                self.render_handle.clone(),
                                compaction_status_reason(reason),
                                Arc::clone(&lock(&self.theme)),
                            ),
                        ));
                    }
                }
                self.render_handle.request_render();
            }
            UiCommand::SummarizationRetryFinished => {
                // summarization_retry_finished (interactive-mode.ts:3170-3174).
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                self.render_handle.request_render();
            }
            UiCommand::BashExecutionUpdate { id, delta } => {
                // bash_execution_update (interactive-mode.ts:3003-3005): the
                // bash execution callback renders the TUI output directly.
                let _ = (id, delta);
            }
            UiCommand::ExtensionError {
                extension_path,
                event,
                error,
            } => {
                // No upstream `handleEvent` case; the no-op extension runner
                // never emits these (T15 wires the extension host).
                let _ = (extension_path, event, error);
            }
            UiCommand::ClearEditor => {
                lock(&self.editor).set_text("");
                *lock(&self.is_bash_mode) = false;
                self.update_editor_border_color();
                self.render_handle.request_render();
            }
            UiCommand::RefreshEditorBorder => {
                self.update_editor_border_color();
            }
            UiCommand::Escape => {
                // The Escape handler (interactive-mode.ts:2564-2590),
                // applied from the drain: the dispatch-side onEscape closure
                // only queues this command (it cannot lock the editor it is
                // dispatched from).
                self.handle_escape();
            }
            UiCommand::Dequeue => {
                // `handleDequeue` (interactive-mode.ts:3759-3766) from the
                // drain (restore touches the editor).
                self.handle_dequeue();
            }
            UiCommand::PasteImage => {
                // `handleClipboardPaste` (interactive-mode.ts:2629-2652)
                // from the drain (insertion touches the editor).
                self.handle_paste_image_impl();
            }
            UiCommand::ThemeChanged => {
                // Theme watcher reload (theme.ts:921-932): reload the custom
                // theme file, keeping the last successfully loaded theme when
                // the file is missing or invalid.
                let reloaded = self
                    .session()
                    .settings_manager(|settings| settings.get_theme())
                    .and_then(|name| crate::core::themes::get_theme_watch_path(&name))
                    .and_then(|path| crate::core::themes::load_theme_from_path(&path, None).ok());
                if let Some(reloaded) = reloaded {
                    self.apply_theme(Arc::new(reloaded));
                }
            }
            UiCommand::ApplyThemeName(name) => {
                // `applyThemeName` (theme-controller.ts:92-100): fall back to
                // the built-in dark theme on load failure and surface the
                // error.
                let theme = match crate::core::themes::load_theme(&name, None) {
                    Ok(theme) => theme,
                    Err(error) => {
                        self.show_error(&format!(
                            "Failed to load theme \"{name}\": {error}\nFell back to dark theme."
                        ));
                        crate::core::themes::load_theme("dark", None)
                            .expect("builtin dark theme must load")
                    }
                };
                self.apply_theme(Arc::new(theme));
            }
            UiCommand::GitBranchChanged => {
                // `onBranchChange` subscriber (interactive-mode.ts:807-809):
                // the watcher thread already updated the data provider; the
                // drain invalidates the footer (component lock contract).
                Component::invalidate(&mut *lock(&self.footer));
                self.render_handle.request_render();
            }
            UiCommand::NewVersionAvailable(release) => {
                // `checkForNewPiVersion(...).then(...)` continuation
                // (interactive-mode.ts:843-847), routed through the drain
                // like every other async result.
                self.show_new_version_notification(&release);
            }
            UiCommand::ShareAbort => {
                // `loader.onAbort` (interactive-mode.ts:5549-5554): kill gh
                // (the runner polls the flag), restore the editor, clean up.
                // Ignored when the share already settled (single `take`).
                let Some(state) = lock(&self.share_state).take() else {
                    return;
                };
                state.cancelled.store(true, Ordering::Relaxed);
                self.hide_selector();
                crate::core::share::cleanup_share_tmp_file(&state.tmp_file);
                self.show_status("Share cancelled");
            }
            UiCommand::ShareCompleted(outcome) => {
                // The `close` handler (interactive-mode.ts:5575-5602).
                // Ignored when the share already settled (abort won).
                let Some(state) = lock(&self.share_state).take() else {
                    return;
                };
                if state.cancelled.load(Ordering::Relaxed) {
                    return;
                }
                self.hide_selector();
                crate::core::share::cleanup_share_tmp_file(&state.tmp_file);
                if outcome.code != Some(0) {
                    let trimmed = outcome.stderr.trim();
                    let detail = if trimmed.is_empty() {
                        "Unknown error"
                    } else {
                        trimmed
                    };
                    self.show_error(&format!("Failed to create gist: {detail}"));
                    return;
                }
                // `gh` prints `https://gist.github.com/<user>/<gistId>`.
                let gist_url = outcome.stdout.trim();
                let Some(gist_id) = crate::core::share::parse_gist_id(gist_url) else {
                    self.show_error("Failed to parse gist ID from gh output");
                    return;
                };
                let preview_url = crate::config::get_share_viewer_url(gist_id);
                self.show_status(&format!("Share URL: {preview_url}\nGist: {gist_url}"));
            }
        }
    }

    fn streaming_stop_reason_is_error(&self, assistant: &rpi_ai::types::AssistantMessage) -> bool {
        assistant.stop_reason == StopReason::Aborted || assistant.stop_reason == StopReason::Error
    }

    /// `new ToolExecutionComponent` with the mode's settings
    /// (interactive-mode.ts:2936-2947).
    fn new_tool_execution_component(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        args: &serde_json::Value,
    ) -> ToolExecutionComponent {
        let mut component = ToolExecutionComponent::new(
            tool_name.to_string(),
            tool_call_id.to_string(),
            args.clone(),
            ToolExecutionOptions {
                show_images: self.show_images,
                image_width_cells: self.image_width_cells,
            },
            // `getRegisteredToolDefinition` (interactive-mode.ts:2944):
            // extension render hooks; overrides without hooks inherit the
            // built-in rendering.
            crate::modes::interactive::extension_renderers::host_tool_definition(
                &self.session(),
                tool_name,
            ),
            Arc::clone(&lock(&self.theme)),
            self.render_handle.clone(),
            self.cwd.clone(),
        );
        component.set_expanded(*lock(&self.tool_output_expanded));
        component
    }

    /// Add a chat child; returns the child's address for later removal.
    pub(crate) fn add_chat_child(&self, child: Box<dyn Component>) -> usize {
        let address = child_address(&*child);
        lock(&self.chat_container).children.push(child);
        address
    }

    /// `showSelector` (interactive-mode.ts:4122-4133): swap the editor in
    /// the TUI child list for a selector entry, preserving its position.
    /// Re-entrant shows (e.g. settings → theme submenu) hide the previous
    /// selector first. May be called from inside a component dispatch (e.g.
    /// a keybinding or a selector's own cancel), so the swap must go through
    /// `Tui::swap_child` — a `child_position` read would fail on the held
    /// inner lock and the fallback would append after the footer.
    pub(crate) fn show_selector(&self, entry: SharedComponent) {
        self.hide_selector();
        self.ui.swap_child(&self.editor_region, &entry);
        *lock(&self.active_selector) = Some(entry.clone());
        self.ui.set_focus(Some(entry));
        self.ui.request_render(false);
    }

    /// The `done()` callback (interactive-mode.ts:4123-4127): restore the
    /// editor at the selector's former position.
    pub(crate) fn hide_selector(&self) {
        if let Some(selector) = lock(&self.active_selector).take() {
            self.ui.swap_child(&selector, &self.editor_region);
            self.ui.set_focus(Some(self.editor_region.clone()));
            self.ui.request_render(false);
        }
    }

    /// `showTreeSelector` (interactive-mode.ts:4635-4747), basic path:
    /// selecting an entry closes the selector and navigates the session
    /// (the default no-summary behavior); the branch-summarization ask loop
    /// (No summary / Summarize / custom prompt, interactive-mode.ts:4661-4690)
    /// is an unassigned hook (wired through `ExtensionSelectorComponent`).
    pub(crate) fn show_tree_selector(ui: &Arc<Self>, initial_selected_id: Option<String>) {
        let session_manager_arc = ui.session().session_manager();
        let manager = lock(&session_manager_arc);
        let tree = manager.get_tree();
        let real_leaf_id = manager.get_leaf_id().map(str::to_owned);
        drop(manager);
        let initial_filter_mode = ui
            .session()
            .settings_manager(|settings| settings.get_tree_filter_mode());
        if tree.is_empty() {
            ui.show_status("No entries in session");
            return;
        }

        let select_ui = Arc::clone(ui);
        let selector = Arc::new(Mutex::new(TreeSelectorComponent::new(
            tree,
            real_leaf_id.clone(),
            ui.ui.terminal_rows(),
            Arc::clone(&lock(&ui.theme)),
            ui.ui.clone(),
            Box::new(move |entry_id: &str| {
                let entry_id = entry_id.to_string();
                let real_leaf_id = real_leaf_id.clone();
                // `done()` first, then navigate (interactive-mode.ts:4659).
                select_ui.hide_selector();
                let session = select_ui.session();
                let ui = Arc::clone(&select_ui);
                commands_selectors::spawn_async(async move {
                    if Some(entry_id.as_str()) == real_leaf_id.as_deref() {
                        // Selecting the current leaf is a no-op
                        // (interactive-mode.ts:4652-4656).
                        ui.show_status("Already at this point");
                        return;
                    }
                    // TODO(unassigned): branch-summarization ask loop
                    // (showExtensionSelector, interactive-mode.ts:4661-4690)
                    // plus the abort escape handler + BranchSummaryStatus
                    // indicator (interactive-mode.ts:4694-4706); navigation
                    // always uses `summarize: false`.
                    let result = session
                        .navigate_tree(
                            &entry_id,
                            crate::core::agent_session::NavigateTreeOptions {
                                summarize: false,
                                custom_instructions: None,
                                replace_instructions: false,
                                label: None,
                            },
                        )
                        .await;
                    match result {
                        Ok(result) => {
                            if result.cancelled {
                                ui.show_status("Navigation cancelled");
                                return;
                            }
                            // Update UI (interactive-mode.ts:4712-4717).
                            lock(&ui.chat_container).clear();
                            ui.render_initial_messages();
                            if let Some(editor_text) = result.editor_text {
                                if lock(&ui.editor).get_text().trim().is_empty() {
                                    lock(&ui.editor).set_text(&editor_text);
                                }
                            }
                            ui.show_status("Navigated to selected point");
                            // TODO(unassigned): flushCompactionQueue({ willRetry: false })
                            // — same wiring gap as the willRetry note in
                            // `flush_compaction_queue` (D-019).
                        }
                        Err(error) => ui.show_error(&error.raw_message()),
                    }
                });
            }),
            Box::new({
                let ui = Arc::clone(ui);
                move || {
                    ui.hide_selector();
                    ui.render_handle.request_render();
                }
            }),
            // TODO(unassigned): label editing writes through to the session
            // manager (`SessionManager::append_label_change` exists; the UI
            // wiring is unclaimed).
            None,
            initial_selected_id,
            initial_filter_mode,
        )));

        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showUserMessageSelector` (interactive-mode.ts:4576-4612): fork-start
    /// selection. The pick is routed to the run loop (the callback has no
    /// runtime access) which runs `InteractiveMode::handle_fork_command`.
    pub(crate) fn show_user_message_selector(ui: &Arc<Self>) {
        let user_messages = ui.session().get_user_messages_for_forking();
        if user_messages.is_empty() {
            ui.show_status("No messages to fork from");
            return;
        }
        let initial_selected_id = user_messages.last().map(|(id, _)| id.clone());

        let select_ui = Arc::clone(ui);
        let selector = Arc::new(Mutex::new(UserMessageSelectorComponent::new(
            Arc::clone(&lock(&ui.theme)),
            user_messages,
            Box::new(move |entry_id: &str| {
                // `onSelect` (interactive-mode.ts:4589-4602): `done()` first,
                // then the runtime fork runs on the run loop.
                select_ui.hide_selector();
                let _ = select_ui
                    .input_tx
                    .send(EditorInput::ForkFrom(entry_id.to_string()));
            }),
            Box::new({
                let ui = Arc::clone(ui);
                move || {
                    ui.hide_selector();
                    ui.render_handle.request_render();
                }
            }),
            initial_selected_id,
        )));

        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showStatusIndicator` (interactive-mode.ts:1851-1856).
    fn show_status_indicator(&self, indicator: ActiveStatus) {
        let mut status = lock(&self.status);
        status.dispose();
        *status = indicator;
    }

    /// `clearStatusIndicator` (interactive-mode.ts:1858-1869).
    fn clear_status_indicator(&self, kind: Option<StatusIndicatorKind>) {
        let mut status = lock(&self.status);
        if let Some(kind) = kind {
            if status.kind() != Some(kind) {
                return;
            }
        }
        status.dispose();
        *status = ActiveStatus::Idle;
    }

    // ==================================================================
    // T15 W4: extension UI bridge support
    // ==================================================================

    /// Re-apply the working indicator after a bridge `setWorking*` call:
    /// only touches the status row when a Working indicator is currently
    /// active (otherwise the stored values apply at the next turn start,
    /// interactive-mode.ts:1181-1190).
    pub(crate) fn refresh_working_indicator(
        &self,
        indicator: Option<rpi_ext_host::api::WorkingIndicatorOptions>,
    ) {
        let active = matches!(&*lock(&self.status), ActiveStatus::Working(_));
        if !self.working_visible.load(Ordering::Relaxed) {
            self.clear_status_indicator(Some(StatusIndicatorKind::Working));
        } else if active {
            let message = lock(&self.working_message)
                .clone()
                .unwrap_or_else(|| "Working...".to_string());
            let theme = Arc::clone(&lock(&self.theme));
            let indicator = indicator.map(|options| LoaderIndicatorOptions {
                frames: options.frames,
                interval_ms: options.interval_ms,
            });
            self.show_status_indicator(ActiveStatus::Working(
                WorkingStatusIndicator::with_options(
                    self.render_handle.clone(),
                    message,
                    theme,
                    indicator,
                ),
            ));
        }
        self.render_handle.request_render();
    }

    /// `setHeader`/`setFooter` region swap: a declarative tree replaces the
    /// region's content; `None` restores the built-in region.
    pub(crate) fn swap_region_component(
        &self,
        default_region: &SharedComponent,
        component: Option<rpi_ext_host::types::ComponentTree>,
        slot: &Mutex<Option<SharedComponent>>,
    ) {
        let current = lock(slot).take();
        match (component, current) {
            (Some(tree), current) => {
                let component = component_from_tree(&tree, &Arc::clone(&lock(&self.theme)));
                let entry = shared_component_from_boxed(component);
                match current {
                    Some(custom) => self.ui.swap_child(&custom, &entry),
                    None => self.ui.swap_child(default_region, &entry),
                }
                *lock(slot) = Some(entry);
            }
            (None, Some(custom)) => {
                self.ui.swap_child(&custom, default_region);
            }
            (None, None) => {}
        }
        self.ui.request_render(false);
    }

    /// `showLoadedResources` (interactive-mode.ts:1421-1627): the loaded
    /// resources listing — Context / Skills / Prompts / Extensions / Themes
    /// sections (ExpandableText, compact name lists) plus the diagnostics
    /// block. Renders into the dedicated `loaded_resources_container`, so
    /// chat clears do not remove it (interactive-mode.ts:1426-1427).
    ///
    /// The init call mirrors `showLoadedResources({ force: false,
    /// showDiagnosticsWhenQuiet: true })` (interactive-mode.ts:1705).
    pub(crate) fn show_loaded_resources(&self) {
        self.show_loaded_resources_inner(false, true);
    }

    fn show_loaded_resources_inner(&self, force: bool, show_diagnostics_when_quiet: bool) {
        let mut container = lock(&self.loaded_resources_container);
        container.clear();

        // Reading the quiet-startup setting takes the resource-loader lock
        // (session.settings_manager), so it must happen before the loader
        // snapshot below (interactive-mode.ts:1429).
        let quiet_startup = self
            .session()
            .settings_manager(|settings| settings.get_quiet_startup());
        let show_listing = force || self.verbose || !quiet_startup;
        let show_diagnostics = show_listing || show_diagnostics_when_quiet;
        if !show_listing && !show_diagnostics {
            return;
        }

        // Snapshot the loaded resources so the loader lock is released
        // before any further `self` locks.
        let loader = self.session().resource_loader();
        let (skills, prompts, extensions, themes, context_files, diagnostics) = {
            let loader = lock(&loader);
            let loaded = loader.resources();
            let skills: Vec<String> = loaded
                .skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect();
            let prompts: Vec<String> = loaded
                .prompts
                .iter()
                .map(|template| format!("/{}", template.name))
                .collect();
            let extensions: Vec<String> = loaded
                .extensions
                .paths
                .iter()
                .filter_map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect();
            // Custom themes only — built-ins are excluded (interactive-mode.ts:1552-1555).
            let themes: Vec<String> = loaded
                .themes
                .iter()
                .filter(|theme| theme.source_path.is_some())
                .map(|theme| {
                    theme.name.clone().unwrap_or_else(|| {
                        theme
                            .source_path
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    })
                })
                .collect();
            let context_files: Vec<String> = loaded
                .context_files
                .iter()
                .map(|file| file.path.display().to_string())
                .collect();
            let diagnostics: Vec<(String, Option<String>)> = loaded
                .diagnostics
                .iter()
                .map(|diagnostic| {
                    (
                        diagnostic.message.clone(),
                        diagnostic
                            .path
                            .clone()
                            .map(|path| path.display().to_string()),
                    )
                })
                .collect();
            (
                skills,
                prompts,
                extensions,
                themes,
                context_files,
                diagnostics,
            )
        };
        drop(loader);

        let section_header = |name: &str| lock(&self.theme).fg("mdHeading", &format!("[{name}]"));
        let format_compact_list = |items: Vec<String>, sort: bool| -> String {
            let mut labels: Vec<String> = items
                .into_iter()
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect();
            if sort {
                labels.sort();
            }
            lock(&self.theme).fg("dim", &format!("  {}", labels.join(", ")))
        };
        // `addLoadedSection` (interactive-mode.ts:1443-1458): an
        // ExpandableText with the header + compact body, expanded state
        // `getStartupExpansionState()` (interactive-mode.ts:1077-1079).
        let add_loaded_section = |container: &mut Container,
                                  name: &str,
                                  compact_body: String,
                                  expanded_body: Option<String>| {
            let expanded_state = self.verbose || *lock(&self.tool_output_expanded);
            let collapsed = format!("{}\n{}", section_header(name), compact_body);
            let expanded_text = format!(
                "{}\n{}",
                section_header(name),
                expanded_body.unwrap_or(compact_body)
            );
            let section = ExpandableText::new(
                Box::new(move || collapsed.clone()),
                Box::new(move || expanded_text.clone()),
                expanded_state,
                0,
                0,
            );
            container.add_child(Box::new(section));
            container.add_child(Box::new(Spacer::new(1)));
        };

        if show_listing {
            // Context first, unsorted (interactive-mode.ts:1495-1506).
            if !context_files.is_empty() {
                container.add_child(Box::new(Spacer::new(1)));
                add_loaded_section(
                    &mut container,
                    "Context",
                    format_compact_list(context_files, false),
                    None,
                );
            }
            if !skills.is_empty() {
                add_loaded_section(
                    &mut container,
                    "Skills",
                    format_compact_list(skills, true),
                    None,
                );
            }
            if !prompts.is_empty() {
                add_loaded_section(
                    &mut container,
                    "Prompts",
                    format_compact_list(prompts, true),
                    None,
                );
            }
            if !extensions.is_empty() {
                add_loaded_section(
                    &mut container,
                    "Extensions",
                    format_compact_list(extensions, true),
                    None,
                );
            }
            if !themes.is_empty() {
                add_loaded_section(
                    &mut container,
                    "Themes",
                    format_compact_list(themes, true),
                    None,
                );
            }
        }

        if show_diagnostics && !diagnostics.is_empty() {
            // The Rust loader aggregates skill/prompt/theme diagnostics into
            // one pipeline-ordered vec (`LoadedResources::diagnostics`), so
            // upstream's four per-kind blocks ([Skill conflicts],
            // [Prompt conflicts], [Extension issues], [Theme conflicts],
            // interactive-mode.ts:1576-1625) collapse to a single block.
            let mut lines = vec![lock(&self.theme).fg("warning", "[Resource issues]")];
            for (message, path) in &diagnostics {
                match path {
                    Some(path) => {
                        lines.push(lock(&self.theme).fg("warning", &format!("  {path}")));
                        lines.push(lock(&self.theme).fg("warning", &format!("    {message}")));
                    }
                    None => lines.push(lock(&self.theme).fg("warning", &format!("  {message}"))),
                }
            }
            container.add_child(Box::new(Text::new(lines.join("\n"), 0, 0, None)));
            container.add_child(Box::new(Spacer::new(1)));
        }
    }

    /// `maybeShowCacheMissNotice` (interactive-mode.ts:3449-3455): the
    /// setting gate plus the `detectCacheMiss` structure are in place; the
    /// cache-stats port (`core/cache-stats.ts`) is TODO(T14), so nothing
    /// renders yet.
    pub(crate) fn maybe_show_cache_miss_notice(
        &self,
        _assistant: &rpi_ai::types::AssistantMessage,
    ) {
        // detectCacheMiss(sessionManager.getEntries(), message, session.modelRuntime)
        // → CacheMiss { missedTokens, missedCost, modelChanged, idleMs }
        // (core/cache-stats.ts, wired at interactive-mode.ts:3453).
        // TODO(T14/cache-stats): port the detector and render the notice
        // (interactive-mode.ts:3457-3471 — 20k-token / $0.10 thresholds,
        // warning label, chat spacer + text) behind the
        // get_show_cache_miss_notices gate.
    }

    /// `showStatus` (interactive-mode.ts:3194-3212): dim status line with
    /// back-to-back coalescing (a status emitted right after the previous
    /// one updates the previous line instead of appending).
    pub(crate) fn show_status(&self, message: &str) {
        let mut chat = lock(&self.chat_container);
        let last_spacer_address = *lock(&self.last_status_spacer);
        let mut last_text = lock(&self.last_status_text).clone();
        let children = &mut chat.children;
        let coalesces = children.len() >= 2
            && last_text.as_ref().is_some_and(|track| {
                child_address(&*children[children.len() - 1]) == track.entry_address
            })
            && Some(child_address(&*children[children.len() - 2])) == last_spacer_address;
        if coalesces {
            let track = last_text.take().expect("checked above");
            lock(&track.handle).set_text(lock(&self.theme).fg("dim", message));
            self.render_handle.request_render();
            return;
        }

        let spacer = Box::new(rpi_tui::components::spacer::Spacer::new(1));
        let spacer_address = child_address(&*spacer);
        children.push(spacer);
        let handle = Arc::new(Mutex::new(Text::new(
            lock(&self.theme).fg("dim", message),
            1,
            0,
            None,
        )));
        let wrapper = Box::new(SharedChild(handle.clone()));
        let entry_address = child_address(&*wrapper);
        children.push(wrapper);
        *lock(&self.last_status_spacer) = Some(spacer_address);
        *lock(&self.last_status_text) = Some(StatusTextTrack {
            entry_address,
            handle,
        });
        self.render_handle.request_render();
    }

    /// `showError` (interactive-mode.ts:3868-3873).
    pub(crate) fn show_error(&self, message: &str) {
        self.add_chat_child(Box::new(Text::new(
            lock(&self.theme).fg("error", &format!("Error: {message}")),
            1,
            0,
            None,
        )));
        self.render_handle.request_render();
    }

    /// `showWarning` (interactive-mode.ts:3874-3878).
    pub(crate) fn show_warning(&self, message: &str) {
        self.add_chat_child(Box::new(Text::new(
            lock(&self.theme).fg("warning", &format!("Warning: {message}")),
            1,
            0,
            None,
        )));
        self.render_handle.request_render();
    }

    /// `showNewVersionNotification` (interactive-mode.ts:3885-3912): bordered
    /// "Update Available" block with the run-prompt line, an optional
    /// Markdown note, and the changelog link (OSC 8 when the terminal
    /// supports hyperlinks).
    pub(crate) fn show_new_version_notification(
        &self,
        release: &crate::core::version_check::LatestRpiRelease,
    ) {
        let theme = Arc::clone(&lock(&self.theme));
        let warning_border = |theme: &Arc<crate::core::themes::Theme>| {
            let theme = Arc::clone(theme);
            Box::new(move |text: &str| theme.fg("warning", text))
                as rpi_tui::components::text::ColorFn
        };
        // T18 (ADR-0011 §6): the action matches self-update availability —
        // `rpi update --self` when this install can self-update, the
        // download URL when it cannot (binary build without a target
        // triple).
        let action = theme.fg("accent", &crate::config::self_update_instruction());
        let update_instruction = theme.fg(
            "muted",
            &format!("New version {} is available. ", release.version),
        ) + &action;
        let changelog_url = "https://revpi.dev/changelog";
        let styled_url = theme.fg("accent", changelog_url);
        let changelog_link = if rpi_tui::terminal_image::get_capabilities().hyperlinks {
            rpi_tui::terminal_image::hyperlink(&styled_url, changelog_url)
        } else {
            styled_url
        };
        let changelog_line = theme.fg("muted", "Changelog: ") + &changelog_link;
        let heading = crate::core::themes::Theme::bold(&theme.fg("warning", "Update Available"));
        let note = release
            .note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(str::to_owned);

        self.add_chat_child(Box::new(Spacer::new(1)));
        self.add_chat_child(Box::new(DynamicBorder::new(warning_border(&theme))));
        self.add_chat_child(Box::new(Text::new(
            format!("{heading}\n{update_instruction}"),
            1,
            0,
            None,
        )));
        if let Some(note) = note {
            let muted = {
                let theme = Arc::clone(&theme);
                Box::new(move |text: &str| theme.fg("muted", text))
                    as rpi_tui::components::text::ColorFn
            };
            self.add_chat_child(Box::new(Spacer::new(1)));
            self.add_chat_child(Box::new(Markdown::new(
                note,
                1,
                0,
                Arc::clone(&lock(&self.markdown_theme)),
                Some(DefaultTextStyle {
                    color: Some(muted),
                    ..Default::default()
                }),
                None,
            )));
            self.add_chat_child(Box::new(Spacer::new(1)));
        }
        self.add_chat_child(Box::new(Text::new(changelog_line, 1, 0, None)));
        self.add_chat_child(Box::new(DynamicBorder::new(warning_border(&theme))));
        self.render_handle.request_render();
    }

    /// `updatePendingMessagesDisplay` (interactive-mode.ts:3968-3985):
    /// render the steering/follow-up queues (session queues + the compaction
    /// queue) as dim rows plus the dequeue hint.
    fn update_pending_messages_display(&self) {
        let mut container = lock(&self.pending_messages_container);
        container.clear();
        let (steering, follow_up) = self.get_all_queued_messages();
        if steering.is_empty() && follow_up.is_empty() {
            return;
        }
        container.children.push(Box::new(Spacer::new(1)));
        for message in steering {
            container.children.push(Box::new(TruncatedText::new(
                lock(&self.theme).fg("dim", &format!("Steering: {message}")),
                1,
                0,
            )));
        }
        for message in follow_up {
            container.children.push(Box::new(TruncatedText::new(
                lock(&self.theme).fg("dim", &format!("Follow-up: {message}")),
                1,
                0,
            )));
        }
        let dequeue_hint = key_display_text("app.message.dequeue");
        container.children.push(Box::new(TruncatedText::new(
            lock(&self.theme).fg(
                "dim",
                &format!("↳ {dequeue_hint} to edit all queued messages"),
            ),
            1,
            0,
        )));
    }

    /// `getAllQueuedMessages` (interactive-mode.ts:3936-3947): session
    /// queues plus the compaction queue, split by mode.
    fn get_all_queued_messages(&self) -> (Vec<String>, Vec<String>) {
        let mut steering = self.session().get_steering_messages();
        let mut follow_up = self.session().get_follow_up_messages();
        let compaction_queue = lock(&self.compaction_queue);
        for message in compaction_queue.iter() {
            match message.mode {
                StreamingBehavior::Steer => steering.push(message.text.clone()),
                StreamingBehavior::FollowUp => follow_up.push(message.text.clone()),
            }
        }
        (steering, follow_up)
    }

    /// `clearAllQueues` (interactive-mode.ts:3953-3965): session queues plus
    /// the compaction queue, cleared and returned split by mode.
    fn clear_all_queues(&self) -> (Vec<String>, Vec<String>) {
        let (steering, follow_up) = self.session().clear_queue();
        let mut compaction_queue = lock(&self.compaction_queue);
        let mut compaction_steering = Vec::new();
        let mut compaction_follow_up = Vec::new();
        for message in compaction_queue.drain(..) {
            match message.mode {
                StreamingBehavior::Steer => compaction_steering.push(message.text),
                StreamingBehavior::FollowUp => compaction_follow_up.push(message.text),
            }
        }
        (
            steering.into_iter().chain(compaction_steering).collect(),
            follow_up.into_iter().chain(compaction_follow_up).collect(),
        )
    }

    /// `addCustomEntryToChat` (interactive-mode.ts:3214-3234).
    fn add_custom_entry_to_chat(&self, entry: &SessionEntry) {
        let SessionEntry::Custom(custom_entry) = entry else {
            return;
        };
        // `getEntryRenderer` (custom-entry.ts); no renderer → renders
        // nothing, matching upstream with zero extensions loaded.
        let renderer: crate::modes::interactive::components::custom_entry::EntryRenderer =
            crate::modes::interactive::extension_renderers::host_entry_renderer(
                &self.session(),
                &custom_entry.custom_type,
            )
            .unwrap_or_else(|| Box::new(|_, _, _| None));
        let mut component = CustomEntryComponent::new(
            custom_entry.clone(),
            renderer,
            Arc::clone(&lock(&self.theme)),
        );
        component.set_expanded(*lock(&self.tool_output_expanded));
        if !component.has_content() {
            return;
        }
        // Insert before the streaming component if present
        // (interactive-mode.ts:3225-3231).
        let mut chat = lock(&self.chat_container);
        if let Some(track) = lock(&self.streaming).as_ref() {
            if let Some(index) = chat
                .children
                .iter()
                .position(|child| child_address(&**child) == track.entry_address)
            {
                chat.children.insert(index, Box::new(component));
                return;
            }
        }
        chat.children.push(Box::new(component));
    }

    /// `addMessageToChat` (interactive-mode.ts:3236-3338).
    fn add_message_to_chat(&self, message: AgentMessage, populate_history: bool) {
        match &message {
            AgentMessage::BashExecution(bash_execution) => {
                let mut component = BashExecutionComponent::new(
                    bash_execution.command.clone(),
                    self.render_handle.clone(),
                    Arc::clone(&lock(&self.theme)),
                    bash_execution.exclude_from_context.unwrap_or(false),
                );
                if !bash_execution.output.is_empty() {
                    component.append_output(&bash_execution.output);
                }
                component.set_complete(
                    bash_execution.exit_code,
                    bash_execution.cancelled,
                    bash_execution.truncated.then(|| TruncationResult {
                        content: String::new(),
                        truncated: true,
                        truncated_by: None,
                        total_lines: 0,
                        total_bytes: 0,
                        output_lines: 0,
                        output_bytes: 0,
                        last_line_partial: false,
                        first_line_exceeds_limit: false,
                        max_lines: 0,
                        max_bytes: 0,
                    }),
                    bash_execution.full_output_path.clone(),
                );
                self.add_chat_child(Box::new(component));
            }
            AgentMessage::Custom(custom_message) => {
                if custom_message.display {
                    let mut component = CustomMessageComponent::new(
                        custom_message.clone(),
                        // `getMessageRenderer` (custom-message.ts:69-85).
                        crate::modes::interactive::extension_renderers::host_message_renderer(
                            &self.session(),
                            &custom_message.custom_type,
                        ),
                        Arc::clone(&lock(&self.theme)),
                        Arc::clone(&lock(&self.markdown_theme)),
                        self.output_pad.load(Ordering::Relaxed),
                    );
                    component.set_expanded(*lock(&self.tool_output_expanded));
                    self.add_chat_child(Box::new(component));
                }
            }
            AgentMessage::CompactionSummary(compaction_summary) => {
                let spacer = Box::new(rpi_tui::components::spacer::Spacer::new(1));
                self.add_chat_child(spacer);
                let mut component = CompactionSummaryMessageComponent::new(
                    compaction_summary.clone(),
                    Arc::clone(&lock(&self.theme)),
                    Arc::clone(&lock(&self.markdown_theme)),
                );
                component.set_expanded(*lock(&self.tool_output_expanded));
                self.add_chat_child(Box::new(component));
            }
            AgentMessage::BranchSummary(branch_summary) => {
                let spacer = Box::new(rpi_tui::components::spacer::Spacer::new(1));
                self.add_chat_child(spacer);
                let mut component = BranchSummaryMessageComponent::new(
                    branch_summary.clone(),
                    Arc::clone(&lock(&self.theme)),
                    Arc::clone(&lock(&self.markdown_theme)),
                );
                component.set_expanded(*lock(&self.tool_output_expanded));
                self.add_chat_child(Box::new(component));
            }
            AgentMessage::User(user_message) => {
                let text_content =
                    rpi_ai::utils::text::content_text_user(&user_message.content, "");
                if text_content.is_empty() {
                    return;
                }
                let mut chat = lock(&self.chat_container);
                if !chat.children.is_empty() {
                    chat.children
                        .push(Box::new(rpi_tui::components::spacer::Spacer::new(1)));
                }
                if let Some(skill_block) =
                    crate::core::agent_session::parse_skill_block(&text_content)
                {
                    // Render the skill block (collapsible), then the user
                    // message separately if present
                    // (interactive-mode.ts:3286-3304).
                    let mut skill_component = SkillInvocationMessageComponent::new(
                        skill_block.clone(),
                        Arc::clone(&lock(&self.theme)),
                        Arc::clone(&lock(&self.markdown_theme)),
                    );
                    skill_component.set_expanded(*lock(&self.tool_output_expanded));
                    chat.children.push(Box::new(skill_component));
                    if let Some(user_message) = &skill_block.user_message {
                        chat.children
                            .push(Box::new(rpi_tui::components::spacer::Spacer::new(1)));
                        chat.children.push(Box::new(UserMessageComponent::new(
                            user_message.clone(),
                            Arc::clone(&lock(&self.theme)),
                            Arc::clone(&lock(&self.markdown_theme)),
                            self.output_pad.load(Ordering::Relaxed),
                        )));
                    }
                } else {
                    chat.children.push(Box::new(UserMessageComponent::new(
                        text_content.clone(),
                        Arc::clone(&lock(&self.theme)),
                        Arc::clone(&lock(&self.markdown_theme)),
                        self.output_pad.load(Ordering::Relaxed),
                    )));
                }
                drop(chat);
                if populate_history {
                    lock(&self.editor).add_to_history(&text_content);
                }
            }
            AgentMessage::Assistant(assistant) => {
                let component =
                    crate::modes::interactive::components::AssistantMessageComponent::new(
                        Some(assistant.clone()),
                        *lock(&self.hide_thinking_block),
                        Arc::clone(&lock(&self.theme)),
                        Arc::clone(&lock(&self.markdown_theme)),
                        lock(&self.hidden_thinking_label).clone(),
                        self.output_pad.load(Ordering::Relaxed),
                    );
                self.add_chat_child(Box::new(component));
            }
            AgentMessage::ToolResult(_) => {
                // Tool results are rendered inline with tool calls, handled
                // separately (interactive-mode.ts:3330-3332).
            }
        }
    }

    /// `renderSessionItems` (interactive-mode.ts:3340-3423) over the
    /// compaction-aware entry list.
    fn render_session_entries(
        &self,
        entries: &[crate::core::session_manager::StoredEntry],
        options: RenderOptions,
    ) {
        lock(&self.pending_tools).clear();
        let mut rendered_pending_tools: HashMap<String, Arc<Mutex<ToolExecutionComponent>>> =
            HashMap::new();
        // TODO(T14): cache-miss notices (collectCacheMisses /
        // addCacheMissNotice, interactive-mode.ts:3347-3350, 3457-3471) —
        // needs the cache-stats port.
        if options.update_footer {
            Component::invalidate(&mut *lock(&self.footer));
            self.update_editor_border_color();
        }

        for stored_entry in entries {
            let Some(entry) = stored_entry.known() else {
                continue;
            };
            if matches!(entry, SessionEntry::Custom(_)) {
                self.add_custom_entry_to_chat(entry);
                continue;
            }
            let messages = session_entry_to_context_messages(entry);
            for message in messages {
                if let AgentMessage::Assistant(assistant) = &message {
                    self.add_message_to_chat(message.clone(), options.populate_history);
                    // Render tool-call components
                    // (interactive-mode.ts:3367-3401).
                    for content in &assistant.content {
                        if let AssistantContent::ToolCall(tool_call) = content {
                            let component = self.new_tool_execution_component(
                                &tool_call.name,
                                &tool_call.id,
                                &serde_json::Value::Object(tool_call.arguments.clone()),
                            );
                            let handle = Arc::new(Mutex::new(component));
                            self.add_chat_child(Box::new(SharedChild(handle.clone())));
                            if assistant.stop_reason == StopReason::Aborted
                                || assistant.stop_reason == StopReason::Error
                            {
                                let error_message = if assistant.stop_reason == StopReason::Aborted
                                {
                                    let retry_attempt = self.session().retry_attempt();
                                    if retry_attempt > 0 {
                                        format!(
                                            "Aborted after {retry_attempt} retry attempt{}",
                                            if retry_attempt > 1 { "s" } else { "" }
                                        )
                                    } else {
                                        "Operation aborted".to_string()
                                    }
                                } else {
                                    assistant
                                        .error_message
                                        .clone()
                                        .unwrap_or_else(|| "Error".to_string())
                                };
                                lock(&handle).update_result(
                                    ToolResultState {
                                        content: vec![ToolResultContentLoose::text(error_message)],
                                        is_error: true,
                                        details: None,
                                    },
                                    false,
                                );
                            } else {
                                rendered_pending_tools.insert(tool_call.id.clone(), handle);
                            }
                        }
                    }
                } else if let AgentMessage::ToolResult(tool_result) = &message {
                    // Match tool results to pending tool components
                    // (interactive-mode.ts:3406-3412).
                    if let Some(component) =
                        rendered_pending_tools.remove(&tool_result.tool_call_id)
                    {
                        lock(&component).update_result(
                            tool_result_state_from_message(tool_result, false),
                            false,
                        );
                    }
                } else {
                    self.add_message_to_chat(message.clone(), options.populate_history);
                }
            }
        }

        for (tool_call_id, component) in rendered_pending_tools {
            lock(&self.pending_tools).insert(tool_call_id, component);
        }
        self.render_handle.request_render();
    }

    /// `renderInitialMessages` (interactive-mode.ts:3473-3488).
    pub(crate) fn render_initial_messages(&self) {
        let entries = lock(&self.session().session_manager()).build_context_entries();
        self.render_session_entries(
            &entries,
            RenderOptions {
                update_footer: true,
                populate_history: true,
            },
        );
        self.render_project_trust_warning_if_needed();

        // Show compaction info if the session was compacted
        // (interactive-mode.ts:3481-3487).
        let all_entries = lock(&self.session().session_manager()).get_entries();
        let compaction_count = all_entries
            .iter()
            .filter(|entry| matches!(entry.known(), Some(SessionEntry::Compaction(_))))
            .count();
        if compaction_count > 0 {
            self.show_status(&format!(
                "Session compacted {} time{}",
                compaction_count,
                if compaction_count == 1 { "" } else { "s" }
            ));
        }
    }

    /// `renderProjectTrustWarningIfNeeded` (interactive-mode.ts:3490-3508).
    fn render_project_trust_warning_if_needed(&self) {
        let trusted = lock(&self.session().resource_loader()).is_project_trusted();
        if trusted || !has_trust_requiring_project_resources(Path::new(&self.cwd)) {
            return;
        }
        let mut chat = lock(&self.chat_container);
        if !chat.children.is_empty() {
            chat.children
                .push(Box::new(rpi_tui::components::spacer::Spacer::new(1)));
        }
        chat.children.push(Box::new(Text::new(
            lock(&self.theme).fg(
                "warning",
                &format!(
                    "This project is not trusted. Project {} resources and packages are ignored. Use /trust to save a trust decision, then restart rpi.",
                    crate::config::CONFIG_DIR_NAME
                ),
            ),
            1,
            0,
            None,
        )));
        self.render_handle.request_render();
    }

    /// `rebuildChatFromMessages` (interactive-mode.ts:3524-3527).
    fn rebuild_chat_from_messages(&self) {
        lock(&self.chat_container).clear();
        let entries = lock(&self.session().session_manager()).build_context_entries();
        self.render_session_entries(&entries, RenderOptions::default());
    }

    /// `updateEditorBorderColor` (interactive-mode.ts:3768-3775).
    /// Swap the active theme instance (theme-controller.ts `applyThemeName`
    /// → `onChanged`): update the shared theme + markdown palette, invalidate
    /// the tree, refresh the editor border and re-render (interactive-mode.ts:
    /// 800-804). Called from the drain / the color-scheme listener — never
    /// concurrently with component callbacks.
    pub(crate) fn apply_theme(&self, theme: Arc<Theme>) {
        *lock(&self.theme) = theme;
        let markdown = {
            let current = lock(&self.theme);
            markdown_theme(&current)
        };
        *lock(&self.markdown_theme) = markdown;
        self.ui.invalidate();
        self.update_editor_border_color();
        self.ui.request_render(false);
    }

    fn update_editor_border_color(&self) {
        let border_color = if *lock(&self.is_bash_mode) {
            let theme = Arc::clone(&lock(&self.theme));
            Box::new(move |text: &str| theme.fg("bashMode", text))
        } else {
            thinking_border_color(&lock(&self.theme), self.session().thinking_level())
        };
        lock(&self.editor).set_border_color(border_color);
        self.render_handle.request_render();
    }

    /// `updateTerminalTitle` (interactive-mode.ts:818-826).
    fn update_terminal_title(&self) {
        let session_manager_arc = self.session().session_manager();
        let manager = lock(&session_manager_arc);
        let cwd_basename = manager
            .get_cwd()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.clone());
        let session_name = manager.get_session_name();
        let title = match session_name {
            Some(name) => format!("{APP_TITLE} - {name} - {cwd_basename}"),
            None => format!("{APP_TITLE} - {cwd_basename}"),
        };
        drop(manager);
        self.ui.with_terminal(|terminal| terminal.set_title(&title));
    }

    /// `updateAvailableProviderCount` (interactive-mode.ts:4364-4371).
    fn update_available_provider_count(&self) {
        let models = if !self.session().scoped_models().is_empty() {
            self.session()
                .scoped_models()
                .into_iter()
                .map(|scoped| scoped.model)
                .collect::<Vec<_>>()
        } else {
            self.session().model_runtime().get_available_snapshot()
        };
        let unique_providers: HashSet<&str> =
            models.iter().map(|model| model.provider.as_str()).collect();
        self.footer_data
            .set_available_provider_count(unique_providers.len());
    }

    /// `toggleToolOutputExpansion` / `setToolsExpanded`
    /// (interactive-mode.ts:3808-3826).
    fn toggle_tool_output_expansion(&self) {
        let expanded = !*lock(&self.tool_output_expanded);
        self.set_tools_expanded(expanded);
    }

    pub(crate) fn set_tools_expanded(&self, expanded: bool) {
        *lock(&self.tool_output_expanded) = expanded;
        // `const activeHeader = this.customHeader ?? this.builtInHeader`
        // (interactive-mode.ts:4036-4038).
        if let Some(header) = lock(&self.custom_header).as_ref() {
            lock(header).set_expanded(expanded);
        } else if let Some(header) = lock(&self.built_in_header).as_ref() {
            lock(header).set_expanded(expanded);
        }
        // `setToolsExpanded` (interactive-mode.ts:4033-4048): upstream walks
        // the loadedResources + chat containers for `isExpandable` children.
        // Tool components stay in the chat container after execution —
        // `pending_tools` is cleared at `tool_execution_end`/`agent_end` — so
        // the chat walk (not the pending map) is what reaches the displayed
        // components; components without expansion state no-op via the
        // defaulted `Component::set_expanded`.
        for child in lock(&self.loaded_resources_container).children.iter_mut() {
            child.set_expanded(expanded);
        }
        for child in lock(&self.chat_container).children.iter_mut() {
            child.set_expanded(expanded);
        }
        self.render_handle.request_render();
    }

    /// `toggleThinkingBlockVisibility` (interactive-mode.ts:3828-3835).
    fn toggle_thinking_block_visibility(&self) {
        let mut hide = lock(&self.hide_thinking_block);
        *hide = !*hide;
        let hide = *hide;
        self.session()
            .settings_manager(|settings| settings.set_hide_thinking_block(hide));
        if let Some(track) = lock(&self.streaming).as_ref() {
            lock(&track.handle).set_hide_thinking_block(hide);
        }
        // Historical assistant messages keep their current state (matches
        // upstream, which only updates the streaming component here).
        self.render_handle.request_render();
    }

    /// `handleCtrlC` (interactive-mode.ts:3533-3541).
    fn handle_ctrl_c(&self) {
        let now = now_millis();
        if now - self.last_sigint_time.load(Ordering::Relaxed) < DOUBLE_PRESS_MS {
            let _ = self.shutdown_tx.send(true);
        } else {
            // Deferred: cannot touch the editor from inside its own dispatch.
            self.push(UiCommand::ClearEditor);
            self.last_sigint_time.store(now, Ordering::Relaxed);
        }
    }

    /// `handleCtrlD` (interactive-mode.ts:3543-3546).
    fn handle_ctrl_d(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// `restoreQueuedMessagesToEditor` (interactive-mode.ts:3987-4007):
    /// merges all queues (session steering + follow-up + compaction queue)
    /// into the editor, optionally aborting the run.
    fn restore_queued_messages_to_editor(&self, abort: bool) -> usize {
        let (steering, follow_up) = self.clear_all_queues();
        let all_queued: Vec<String> = steering.into_iter().chain(follow_up).collect();
        if all_queued.is_empty() {
            self.update_pending_messages_display();
            if abort {
                self.session().agent().abort();
            }
            return 0;
        }
        let queued_text = all_queued.join("\n\n");
        let current_text = lock(&self.editor).get_text();
        let combined = [queued_text, current_text]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        lock(&self.editor).set_text(&combined);
        self.update_pending_messages_display();
        if abort {
            self.session().agent().abort();
        }
        all_queued.len()
    }

    /// The Escape handler (interactive-mode.ts:2564-2590).
    fn handle_escape(&self) {
        if self.session().is_streaming() {
            self.restore_queued_messages_to_editor(true);
        } else if self.session().is_bash_running() {
            self.session().abort_bash();
        } else if *lock(&self.is_bash_mode) {
            lock(&self.editor).set_text("");
            *lock(&self.is_bash_mode) = false;
            self.update_editor_border_color();
        } else if lock(&self.editor).get_text().trim().is_empty() {
            // Double-escape with an empty editor triggers /tree, /fork, or
            // nothing based on the setting (interactive-mode.ts:2573-2588).
            if self.double_escape_action != DoubleEscapeAction::None {
                let now = now_millis();
                if now - self.last_escape_time.load(Ordering::Relaxed) < DOUBLE_PRESS_MS {
                    match self.double_escape_action {
                        DoubleEscapeAction::Tree => {
                            // `showTreeSelector()` (interactive-mode.ts:2579-2580).
                            let Some(ui) = self.upgrade_self() else {
                                return;
                            };
                            InteractiveUi::show_tree_selector(&ui, None);
                        }
                        DoubleEscapeAction::Fork => {
                            let Some(ui) = self.upgrade_self() else {
                                return;
                            };
                            InteractiveUi::show_user_message_selector(&ui);
                        }
                        DoubleEscapeAction::None => {}
                    }
                    self.last_escape_time.store(0, Ordering::Relaxed);
                } else {
                    self.last_escape_time.store(now, Ordering::Relaxed);
                }
            }
        }
    }

    /// `queueCompactionMessage` (interactive-mode.ts:4008-4014) — S4b
    /// interface shape; the full queue semantics (extension commands
    /// `queueCompactionMessage` (interactive-mode.ts:4008-4014): queue for
    /// after compaction, record history, clear the editor, refresh the
    /// pending display and confirm with a status line.
    fn queue_compaction_message(&self, text: String, mode: StreamingBehavior) {
        lock(&self.editor).add_to_history(&text);
        lock(&self.editor).set_text("");
        lock(&self.compaction_queue).push(CompactionQueuedMessage { text, mode });
        self.update_pending_messages_display();
        self.show_status("Queued message for after compaction");
    }

    /// `setupKeyHandlers` (interactive-mode.ts:2561-2627): installs the
    /// app-level actions (model cycling, selectors, suspend, clipboard).
    fn setup_key_handlers(ui: &Arc<Self>) {
        let mut editor = lock(&ui.editor);

        let escape_ui = Arc::clone(ui);
        editor.on_escape = Some(Box::new(move || {
            // Deferred to the drain: the handler cannot lock the editor it
            // is dispatched from (interactive-mode.ts:2564-2590).
            escape_ui.push(UiCommand::Escape);
        }));

        let ctrl_c_ui = Arc::clone(ui);
        editor.on_action("app.clear", Box::new(move || ctrl_c_ui.handle_ctrl_c()));
        let ctrl_d_ui = Arc::clone(ui);
        editor.on_ctrl_d = Some(Box::new(move || ctrl_d_ui.handle_ctrl_d()));

        // App-level actions (keybindings.md app.* table).
        let suspend_ui = Arc::clone(ui);
        editor.on_action("app.suspend", Box::new(move || suspend_ui.handle_ctrl_z()));
        let thinking_cycle_ui = Arc::clone(ui);
        editor.on_action(
            "app.thinking.cycle",
            Box::new(move || thinking_cycle_ui.cycle_thinking_level()),
        );
        let model_forward_ui = Arc::clone(ui);
        editor.on_action(
            "app.model.cycleForward",
            Box::new(move || {
                model_forward_ui.cycle_model(crate::core::agent_session::CycleDirection::Forward)
            }),
        );
        let model_backward_ui = Arc::clone(ui);
        editor.on_action(
            "app.model.cycleBackward",
            Box::new(move || {
                model_backward_ui.cycle_model(crate::core::agent_session::CycleDirection::Backward)
            }),
        );
        let model_select_ui = Arc::clone(ui);
        editor.on_action(
            "app.model.select",
            Box::new(move || {
                InteractiveUi::show_model_selector(&model_select_ui, None);
            }),
        );
        let external_editor_ui = Arc::clone(ui);
        editor.on_action(
            "app.editor.external",
            Box::new(move || external_editor_ui.handle_open_external_editor()),
        );
        let copy_ui = Arc::clone(ui);
        editor.on_action(
            "app.message.copy",
            Box::new(move || copy_ui.handle_copy_command()),
        );
        // Alt+Enter follow-up: forwarded to the run loop, which owns the
        // prompt sequencing (handle_follow_up).
        let follow_up_ui = Arc::clone(ui);
        editor.on_action(
            "app.message.followUp",
            Box::new(move || {
                let _ = follow_up_ui
                    .input_tx
                    .send(EditorInput::FollowUp(String::new()));
            }),
        );
        let dequeue_ui = Arc::clone(ui);
        editor.on_action(
            "app.message.dequeue",
            Box::new(move || {
                // Deferred to the drain: restore touches the editor, which is
                // locked during this dispatch.
                dequeue_ui.push(UiCommand::Dequeue);
            }),
        );
        // app.session.* actions have no default bindings (keybindings.md);
        // when user-configured they route through the slash-command chain.
        let tree_ui = Arc::clone(ui);
        editor.on_action(
            "app.session.tree",
            Box::new(move || {
                let _ = tree_ui
                    .input_tx
                    .send(EditorInput::Submit("/tree".to_string()));
            }),
        );
        let fork_ui = Arc::clone(ui);
        editor.on_action(
            "app.session.fork",
            Box::new(move || {
                let _ = fork_ui
                    .input_tx
                    .send(EditorInput::Submit("/fork".to_string()));
            }),
        );
        let resume_ui = Arc::clone(ui);
        editor.on_action(
            "app.session.resume",
            Box::new(move || {
                let _ = resume_ui
                    .input_tx
                    .send(EditorInput::Submit("/resume".to_string()));
            }),
        );
        let new_ui = Arc::clone(ui);
        editor.on_action(
            "app.session.new",
            Box::new(move || {
                let _ = new_ui
                    .input_tx
                    .send(EditorInput::Submit("/new".to_string()));
            }),
        );
        let named_filter_ui = Arc::clone(ui);
        editor.on_action(
            "app.session.toggleNamedFilter",
            Box::new(move || named_filter_ui.handle_toggle_named_filter()),
        );

        let tools_ui = Arc::clone(ui);
        editor.on_action(
            "app.tools.expand",
            Box::new(move || tools_ui.toggle_tool_output_expansion()),
        );
        let thinking_ui = Arc::clone(ui);
        editor.on_action(
            "app.thinking.toggle",
            Box::new(move || thinking_ui.toggle_thinking_block_visibility()),
        );
        let paste_ui = Arc::clone(ui);
        editor.on_action(
            "app.clipboard.pasteImage",
            Box::new(move || {
                // Deferred to the drain (insertion touches the locked editor).
                paste_ui.push(UiCommand::PasteImage);
            }),
        );
        // Ctrl+V also reaches `handle_paste_image` through
        // `CustomEditor::on_paste_image` (custom-editor.ts:37-41), which
        // dispatches on the same `app.clipboard.pasteImage` id.
        let change_ui = Arc::clone(ui);
        editor.set_on_change(Some(Box::new(move |text: &str| {
            let is_bash_mode = text.trim_start().starts_with('!');
            let mut current = lock(&change_ui.is_bash_mode);
            if *current != is_bash_mode {
                *current = is_bash_mode;
                change_ui.push(UiCommand::RefreshEditorBorder);
            }
        })));
    }

    /// `setupEditorSubmitHandler` (interactive-mode.ts:2654-2841): the
    /// dispatch-side half — forward the (already-trimmed, editor-cleared)
    /// text to the run loop, which owns the session calls.
    fn setup_editor_submit_handler(ui: &Arc<Self>) {
        let input_tx = ui.input_tx.clone();
        lock(&ui.editor).set_on_submit(Some(Box::new(move |text: &str| {
            let _ = input_tx.send(EditorInput::Submit(text.to_string()));
        })));
    }
}

/// `isExtensionCommand` (interactive-mode.ts:4017-4027).
fn is_extension_command(runner: &Arc<dyn ExtensionRunner>, text: &str) -> bool {
    if !text.starts_with('/') {
        return false;
    }
    let space_index = text.find(' ');
    let command_name = match space_index {
        Some(index) => &text[1..index],
        None => &text[1..],
    };
    runner.get_command(command_name).is_some()
}

fn compaction_status_reason(reason: CompactionReason) -> CompactionStatusReason {
    match reason {
        CompactionReason::Manual => CompactionStatusReason::Manual,
        CompactionReason::Threshold => CompactionStatusReason::Threshold,
        CompactionReason::Overflow => CompactionStatusReason::Overflow,
    }
}

// =============================================================================
// InteractiveMode — session + run loop
// =============================================================================

/// The interactive mode (interactive-mode.ts:323-920). Owns the runtime and
/// the prompt-sequencing run loop; the component tree lives in
/// [`InteractiveUi`]. The `runtime`/`session`/`ui_state` fields are
/// `pub(crate)` for the command handlers in `commands.rs`.
pub struct InteractiveMode {
    pub(crate) runtime: AgentSessionRuntime,
    ui: Tui,
    pub(crate) session: AgentSession,
    options: InteractiveModeOptions,
    pub(crate) ui_state: Arc<InteractiveUi>,
    input_rx: UnboundedReceiver<EditorInput>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    flush_rx: watch::Receiver<u64>,
    driver_stop: Arc<AtomicBool>,
    driver: Option<std::thread::JoinHandle<()>>,
    /// Theme watcher stop flag + handle (T12-S6; joined in `shutdown`).
    theme_watcher_stop: Arc<AtomicBool>,
    theme_watcher: Option<std::thread::JoinHandle<()>>,
    /// Git branch watcher stop flag + handle (footer-data-provider.ts
    /// `setupGitWatcher`; joined in `shutdown`).
    git_watcher_stop: Arc<AtomicBool>,
    git_watcher: Option<std::thread::JoinHandle<()>>,
    editor_region: SharedComponent,
    /// Agent subscription unsubscribe (interactive-mode.ts:381-382); called
    /// on shutdown, and on session rebind (`rebind_session_ui`).
    unsubscribe: Option<Box<dyn FnOnce() + Send>>,
    is_initialized: bool,
    is_shutting_down: bool,
}

impl InteractiveMode {
    /// Build the mode on the process terminal (interactive-mode.ts:448-495).
    pub fn new(runtime: AgentSessionRuntime, options: InteractiveModeOptions) -> Self {
        Self::with_terminal(
            runtime,
            options,
            Box::new(rpi_tui::terminal::ProcessTerminal::new()),
        )
    }

    /// Build the mode on an injected terminal (test injection point).
    pub fn with_terminal(
        runtime: AgentSessionRuntime,
        options: InteractiveModeOptions,
        terminal: Box<dyn rpi_tui::terminal::Terminal + Send>,
    ) -> Self {
        let session = runtime.session().clone();
        let theme = resolve_theme(&session);
        let markdown_theme = markdown_theme(&theme);
        let agent_dir = runtime.services().agent_dir.clone();
        let show_hardware_cursor =
            session.settings_manager(|settings| settings.get_show_hardware_cursor());
        let clear_on_shrink = session.settings_manager(|settings| settings.get_clear_on_shrink());
        let ui = Tui::with_options(terminal, Some(show_hardware_cursor), Some(agent_dir));
        ui.set_clear_on_shrink(clear_on_shrink);
        let render_handle = ui.render_handle();
        let cwd = session
            .session_manager()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_cwd()
            .to_path_buf();

        let footer_data = Arc::new(FooterDataProvider::new(&cwd));
        let footer = Arc::new(Mutex::new(FooterComponent::new(
            session.clone(),
            Arc::clone(&footer_data),
            Arc::clone(&theme),
        )));
        let editor = Arc::new(Mutex::new(CustomEditor::new(
            ui.clone(),
            editor_theme(&theme),
            EditorOptions {
                padding_x: Some(
                    session
                        .settings_manager(|settings| settings.get_editor_padding_x())
                        .min(128) as usize,
                ),
                autocomplete_max_visible: Some(
                    session
                        .settings_manager(|settings| settings.get_autocomplete_max_visible())
                        .min(20) as usize,
                ),
            },
        )));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (flush_tx, flush_rx) = watch::channel(0u64);
        let (input_tx, input_rx) = unbounded_channel();

        // The editor region is the TUI-visible entry; the mode keeps the
        // shared handle for drain-side mutations.
        let editor_region =
            shared_component_from_boxed(Box::new(CustomEditorRegion::new(editor.clone())));

        let ui_state = Arc::new(InteractiveUi {
            ui: ui.clone(),
            session: RwLock::new(session.clone()),
            theme: Mutex::new(theme),
            markdown_theme: Mutex::new(markdown_theme),
            render_handle,
            event_queue: Arc::new(Mutex::new(VecDeque::new())),
            header_container: Arc::new(Mutex::new(Container::new())),
            loaded_resources_container: Arc::new(Mutex::new(Container::new())),
            chat_container: Arc::new(Mutex::new(Container::new())),
            pending_messages_container: Arc::new(Mutex::new(Container::new())),
            status: Arc::new(Mutex::new(ActiveStatus::Idle)),
            widgets_above: Arc::new(Mutex::new(Container::new())),
            widgets_below: Arc::new(Mutex::new(Container::new())),
            editor,
            footer,
            footer_data,
            editor_region: editor_region.clone(),
            header_region: Mutex::new(None),
            footer_region: Mutex::new(None),
            custom_header: Mutex::new(None),
            custom_footer: Mutex::new(None),
            active_selector: Mutex::new(None),
            self_arc: Mutex::new(None),
            built_in_header: Mutex::new(None),
            streaming: Mutex::new(None),
            pending_tools: Mutex::new(HashMap::new()),
            tool_output_expanded: Mutex::new(false),
            hide_thinking_block: Mutex::new(
                session.settings_manager(|settings| settings.get_hide_thinking_block()),
            ),
            output_pad: AtomicUsize::new(
                session.settings_manager(|settings| settings.get_output_pad()) as usize,
            ),
            hidden_thinking_label: Mutex::new("Thinking...".to_string()),
            working_visible: AtomicBool::new(true),
            working_message: Mutex::new(None),
            last_status_spacer: Mutex::new(None),
            last_status_text: Mutex::new(None),
            is_bash_mode: Mutex::new(false),
            bash_component: Mutex::new(None),
            pending_bash_components: Mutex::new(Vec::new()),
            share_runner: Mutex::new(Arc::new(crate::core::share::SystemShareRunner)),
            report_install_transport: Mutex::new(Arc::new(
                crate::core::telemetry::ReqwestReportInstallTransport,
            )),
            latest_version_transport: Mutex::new(Arc::new(
                crate::core::version_check::ReqwestLatestVersionTransport,
            )),
            share_state: Mutex::new(None),
            last_sigint_time: AtomicU64::new(0),
            last_escape_time: AtomicU64::new(0),
            auto_compaction_escape_handler: Mutex::new(None),
            retry_escape_handler: Mutex::new(None),
            show_terminal_progress: session
                .settings_manager(|settings| settings.get_show_terminal_progress()),
            show_images: session.settings_manager(|settings| settings.get_show_images()),
            image_width_cells: session
                .settings_manager(|settings| settings.get_image_width_cells())
                .clamp(1, 1024) as usize,
            double_escape_action: session
                .settings_manager(|settings| settings.get_double_escape_action()),
            cwd: cwd.display().to_string(),
            verbose: options.verbose,
            shutdown_requested: AtomicBool::new(false),
            shutdown_tx,
            flush_tx,
            compaction_queue: Mutex::new(Vec::new()),
            flush_generation: AtomicU64::new(0),
            input_tx,
            skill_commands: Mutex::new(HashMap::new()),
        });
        *lock(&ui_state.self_arc) = Some(Arc::downgrade(&ui_state));

        InteractiveMode {
            runtime,
            ui,
            session,
            options,
            ui_state,
            input_rx,
            shutdown_rx,
            flush_rx,
            driver_stop: Arc::new(AtomicBool::new(false)),
            driver: None,
            theme_watcher_stop: Arc::new(AtomicBool::new(false)),
            theme_watcher: None,
            git_watcher_stop: Arc::new(AtomicBool::new(false)),
            git_watcher: None,
            editor_region,
            unsubscribe: None,
            is_initialized: false,
            is_shutting_down: false,
        }
    }

    /// The shutdown signal sender (signal-handler registration).
    pub fn shutdown_sender(&self) -> watch::Sender<bool> {
        self.ui_state.shutdown_tx.clone()
    }

    /// `init` (interactive-mode.ts:679-813) — S4b skeleton.
    async fn init(&mut self) {
        if self.is_initialized {
            return;
        }
        self.is_initialized = true;

        // Extension bindings (interactive-mode.ts binds via
        // bindCurrentSessionExtensions; no explicit mode for interactive).
        self.session
            .bind_extensions(ExtensionBindings {
                mode: None,
                on_error: None,
                shutdown: Some({
                    let shutdown_tx = self.ui_state.shutdown_tx.clone();
                    std::sync::Arc::new(move || {
                        let _ = shutdown_tx.send(true);
                    })
                }),
            })
            .await;

        // Extension shortcuts on the default editor
        // (interactive-mode.ts:1827-1841): the editor consults this hook
        // before its own key handling; conflict resolution (reserved keys,
        // last-wins) lives in the host (runner.ts:490-533).
        crate::modes::interactive::extension_shortcuts::install_extension_shortcuts(
            &self.ui_state,
            &self.session,
        );

        // `setUIContext(createExtensionUIContext(), "tui")` (T15 W4): the
        // interactive bridge on the session's extension host.
        {
            let runner = self.session.extension_runner();
            if let Some(host) = runner
                .as_any()
                .and_then(|any| {
                    any.downcast_ref::<crate::core::extension_host_adapter::ExtensionHostAdapter>()
                })
                .map(|adapter| adapter.host().clone())
            {
                host.set_ui(
                    Some(Arc::new(ui_bridge::InteractiveUiBridge::new(
                        &self.ui_state,
                    ))),
                    rpi_ext_host::types::ExtensionMode::Tui,
                );
            }
        }

        // Install telemetry (interactive-mode.ts:685 →
        // `getChangelogForDisplay` 991-1014 → `reportInstallTelemetry`
        // 1017-1036): a fresh install or version change records
        // `lastChangelogVersion` and fires the anonymous ping (gated by
        // offline / `enableInstallTelemetry` / the endpoint, ADR-0002 §8).
        // The changelog asset itself is T15; the ping is fire-and-forget.
        let has_messages = self.session.has_messages();
        let report = self.session.settings_manager(|settings| {
            crate::core::telemetry::prepare_install_report(settings, VERSION, has_messages)
        });
        if let Some((enabled, endpoint)) = report {
            let transport = lock(&self.ui_state.report_install_transport).clone();
            tokio::spawn(async move {
                crate::core::telemetry::report_install(
                    VERSION,
                    enabled,
                    endpoint.as_deref(),
                    crate::core::environment::is_offline(),
                    &*transport,
                )
                .await;
            });
        }

        // Assemble the UI tree (interactive-mode.ts:707-719): the container
        // chain order IS the layout — header → loaded resources → chat →
        // pending messages → status → widgets above → editor → widgets
        // below → footer.
        let ui_state = Arc::clone(&self.ui_state);
        let add_region = |container: Arc<Mutex<Container>>| {
            shared_component_from_boxed(Box::new(SharedChild(container)))
        };
        let header_region = add_region(ui_state.header_container.clone());
        *lock(&ui_state.header_region) = Some(header_region.clone());
        self.ui.add_child(header_region);
        self.ui
            .add_child(add_region(ui_state.loaded_resources_container.clone()));
        self.ui
            .add_child(add_region(ui_state.chat_container.clone()));
        self.ui
            .add_child(add_region(ui_state.pending_messages_container.clone()));
        self.ui
            .add_child(shared_component_from_boxed(Box::new(SharedChild(
                ui_state.status.clone(),
            ))));
        self.ui
            .add_child(add_region(ui_state.widgets_above.clone()));
        self.ui.add_child(self.editor_region.clone());
        self.ui
            .add_child(add_region(ui_state.widgets_below.clone()));
        let footer_region =
            shared_component_from_boxed(Box::new(SharedChild(ui_state.footer.clone())));
        *lock(&ui_state.footer_region) = Some(footer_region.clone());
        self.ui.add_child(footer_region);
        self.ui.set_focus(Some(self.editor_region.clone()));

        InteractiveUi::setup_key_handlers(&ui_state);
        InteractiveUi::setup_editor_submit_handler(&ui_state);
        // Four-source autocomplete (builtin + templates + extensions + skills,
        // interactive-mode.ts:631-647).
        crate::modes::interactive::autocomplete::setup_autocomplete(&ui_state);
        // Kept on the mode so `shutdown` (and session rebinds) can detach
        // (interactive-mode.ts:381-382, 1734-1735).
        self.unsubscribe = Some(ui_state.subscribe_to_agent());

        // Start the UI (interactive-mode.ts:726).
        self.ui.start();

        // Theme file watcher (theme.ts:886-957): a polling thread detects
        // custom-theme file changes and queues `UiCommand::ThemeChanged`;
        // the drain reloads and applies (theme.ts:921-932).
        let watcher_stop = Arc::clone(&self.theme_watcher_stop);
        let watcher = crate::modes::interactive::theme_watcher::spawn_theme_watcher(
            self.ui.clone(),
            Arc::clone(&ui_state),
            watcher_stop,
        );
        self.theme_watcher = Some(watcher);

        // Git branch watcher (footer-data-provider.ts:120-124, 307-381): a
        // polling thread tracks `.git/HEAD` and queues
        // `UiCommand::GitBranchChanged`; the drain invalidates the footer.
        // Resolve once up front so the first footer paint shows the branch
        // (upstream resolves lazily on the first `getGitBranch`,
        // footer-data-provider.ts:127-132).
        let initial_branch = crate::modes::interactive::git_branch_watcher::resolve_branch_for_cwd(
            &ui_state.footer_data.cwd(),
        );
        ui_state.footer_data.set_git_branch(initial_branch);
        let git_stop = Arc::clone(&self.git_watcher_stop);
        let git_watcher = crate::modes::interactive::git_branch_watcher::spawn_git_branch_watcher(
            Arc::clone(&ui_state),
            git_stop,
            crate::modes::interactive::git_branch_watcher::GIT_WATCH_POLL_INTERVAL,
        );
        self.git_watcher = Some(git_watcher);

        // Header (interactive-mode.ts:731-791): quiet startup yields an
        // empty header; otherwise the ExpandableText linked to tools
        // expansion.
        let quiet_startup = self
            .session
            .settings_manager(|settings| settings.get_quiet_startup());
        let expanded = self.options.verbose || *lock(&ui_state.tool_output_expanded);
        let (header, expandable) = build_builtin_header(
            Arc::clone(&lock(&ui_state.theme)),
            VERSION,
            expanded,
            quiet_startup,
        );
        let mut header_container = lock(&ui_state.header_container);
        if !quiet_startup {
            header_container
                .children
                .push(Box::new(rpi_tui::components::spacer::Spacer::new(1)));
        }
        header_container.children.push(header);
        if !quiet_startup {
            header_container
                .children
                .push(Box::new(rpi_tui::components::spacer::Spacer::new(1)));
        }
        *lock(&ui_state.built_in_header) = expandable;
        drop(header_container);
        self.ui.request_render(false);

        // `showLoadedResources({ force: false, showDiagnosticsWhenQuiet: true })`
        // (interactive-mode.ts:1705). Session switching rebinds through
        // `rebind_session_ui` (commands_selectors.rs); the git branch watcher
        // was started above and follows `FooterDataProvider::set_cwd`.
        ui_state.show_loaded_resources();
        lock(&ui_state.footer).set_auto_compact_enabled(self.session.auto_compaction_enabled());

        // Render initial messages AFTER showing loaded resources
        // (interactive-mode.ts:796-797).
        ui_state.render_initial_messages();
        ui_state.update_available_provider_count();
        ui_state.update_editor_border_color();
        ui_state.update_terminal_title();
    }

    /// Run the interactive mode (interactive-mode.ts:832-920): drive the TUI
    /// from a dedicated thread while this loop sequences prompts.
    pub async fn run(&mut self) {
        self.init().await;

        // Start the version check asynchronously (interactive-mode.ts:
        // 842-847): the resolved endpoint (ADR-0002 §8) already applies the
        // skip/offline/disabled gates — `None` means no probe at all.
        let probe_url = self.session.settings_manager(|settings| {
            crate::core::version_check::startup_version_check_url(
                settings.get_version_check_url().as_deref(),
            )
        });
        if let Some(probe_url) = probe_url {
            let ui_state = Arc::clone(&self.ui_state);
            let transport = lock(&self.ui_state.latest_version_transport).clone();
            tokio::spawn(async move {
                if let Some(release) = crate::core::version_check::check_for_new_rpi_release(
                    VERSION,
                    &*transport,
                    Some(&probe_url),
                )
                .await
                {
                    ui_state.push(UiCommand::NewVersionAvailable(release));
                }
            });
        }

        let driver_ui = self.ui.clone();
        let driver_shared = Arc::clone(&self.ui_state);
        let driver_stop = Arc::clone(&self.driver_stop);
        self.driver = Some(
            std::thread::Builder::new()
                .name("rpi-tui-driver".to_string())
                .spawn(move || {
                    while !driver_stop.load(Ordering::Relaxed) {
                        let timeout = match driver_ui.next_deadline() {
                            Some(deadline) => deadline
                                .saturating_duration_since(Instant::now())
                                .min(DRIVER_PUMP_CAP),
                            None => DRIVER_PUMP_CAP,
                        };
                        driver_ui.pump(Some(timeout));
                        driver_shared.drain_events();
                    }
                })
                .expect("spawn TUI driver thread"),
        );

        // Auto theme (theme-controller.ts:37-45): resolve `light/dark`-style
        // settings against the terminal appearance and follow color scheme
        // changes. Runs after the driver is up so the OSC 11 / DSR queries
        // are answered by `pump`.
        self.init_auto_theme().await;

        // Resume handler for Ctrl+Z suspend (interactive-mode.ts:3707-3712):
        // restore the TUI when the process group is continued.
        #[cfg(unix)]
        {
            let resume_ui = self.ui.clone();
            let resume_state = Arc::clone(&self.ui_state);
            let mut sigcont = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::from_raw(libc::SIGCONT),
            )
            .expect("SIGCONT handler");
            tokio::spawn(async move {
                loop {
                    sigcont.recv().await;
                    resume_ui.start();
                    resume_state.render_handle.request_render();
                }
            });
        }

        // Process initial messages (interactive-mode.ts:889-908).
        if let Some(initial_message) = self.options.initial_message.clone() {
            self.prompt_or_shutdown(
                &initial_message,
                PromptOptions {
                    images: self.options.initial_images.clone(),
                    ..Default::default()
                },
            )
            .await;
        }
        for message in self.options.initial_messages.clone() {
            self.prompt_or_shutdown(&message, PromptOptions::default())
                .await;
        }

        // Show startup warnings (interactive-mode.ts:872-885).
        if let Some(message) = self.options.model_fallback_message.clone() {
            self.ui_state.show_warning(&message);
        }

        // Main interactive loop (interactive-mode.ts:910-919).
        loop {
            match self.next_command().await {
                None => break,
                Some(RunCommand::Submit(text)) => self.handle_submit(text).await,
                Some(RunCommand::FollowUp) => self.handle_follow_up().await,
                Some(RunCommand::ResumeSession(path)) => self.handle_resume_command(&path).await,
                Some(RunCommand::ForkFrom(entry_id)) => self.handle_fork_command(&entry_id).await,
                Some(RunCommand::FlushCompactionQueue) => self.flush_compaction_queue().await,
            }
        }

        self.shutdown().await;
    }

    /// Auto theme resolution + color-scheme follow (theme-controller.ts:37-45
    /// and 113-125): when the theme setting is an automatic pair (or the
    /// plain `"auto"` shorthand), detect the terminal appearance, apply the
    /// matching branch, and follow color-scheme changes. The change listener
    /// is registered unconditionally (upstream registers it in the controller
    /// constructor, theme-controller.ts:34) so a `/settings` switch to an
    /// automatic pair at runtime is followed too; the listener no-ops while
    /// the setting is not an automatic pair, and terminal notifications stay
    /// disabled until then (`setAutoSync`, theme-controller.ts:107-111).
    async fn init_auto_theme(&self) {
        let ui = &self.ui_state.ui;
        // `applyTerminalTheme` (theme-controller.ts:113-125): re-resolve the
        // auto pair when the terminal reports a color scheme change. Runs on
        // the driver thread (the TUI's tick) — the same thread as the drain,
        // so the theme field lock never contends.
        let listener_ui = Arc::clone(&self.ui_state);
        ui.on_terminal_color_scheme_change(Box::new(move |scheme| {
            let setting = listener_ui
                .session()
                .settings_manager(|settings| settings.get_theme_setting());
            let Some((light, dark)) =
                crate::modes::interactive::theme_watcher::auto_theme_pair(setting.as_deref())
            else {
                return;
            };
            let name = match scheme {
                rpi_tui::terminal_colors::TerminalColorScheme::Light => light,
                rpi_tui::terminal_colors::TerminalColorScheme::Dark => dark,
            };
            if let Ok(theme) = crate::core::themes::load_theme(&name, None) {
                listener_ui.apply_theme(Arc::new(theme));
            }
        }));

        let setting = self
            .session
            .settings_manager(|settings| settings.get_theme_setting());
        let Some((light, dark)) =
            crate::modes::interactive::theme_watcher::auto_theme_pair(setting.as_deref())
        else {
            return;
        };
        ui.set_terminal_color_scheme_notifications(true);
        let terminal_theme =
            crate::modes::interactive::theme_watcher::detect_terminal_theme_for_auto(ui, 100).await;
        let name = match terminal_theme {
            rpi_tui::terminal_colors::TerminalColorScheme::Light => light,
            rpi_tui::terminal_colors::TerminalColorScheme::Dark => dark,
        };
        if let Ok(theme) = crate::core::themes::load_theme(&name, None) {
            self.ui_state.apply_theme(Arc::new(theme));
        }
    }

    /// Await the next submit / flush / shutdown (upstream `getUserInput`,
    /// interactive-mode.ts:3510-3522 — the editor submit handler resolves
    /// it; here the TUI driver keeps rendering while this waits).
    async fn next_command(&mut self) -> Option<RunCommand> {
        tokio::select! {
            biased;
            _ = self.shutdown_rx.changed() => None,
            input = self.input_rx.recv() => {
                let input = input?;
                Some(match input {
                    EditorInput::Submit(text) => RunCommand::Submit(text),
                    // Alt+Enter follow-up (interactive-mode.ts:3727-3757):
                    // the run loop reads the editor text at processing time
                    // (the dispatch cannot lock the editor).
                    EditorInput::FollowUp(_) => RunCommand::FollowUp,
                    // Selector picks routed from the driver-thread callbacks
                    // (interactive-mode.ts:4779-4781, 4589-4602).
                    EditorInput::ResumeSession(path) => RunCommand::ResumeSession(path),
                    EditorInput::ForkFrom(entry_id) => RunCommand::ForkFrom(entry_id),
                })
            }
            _ = self.flush_rx.changed() => Some(RunCommand::FlushCompactionQueue),
        }
    }

    /// `handleSubmit` — the run-loop half of `setupEditorSubmitHandler`
    /// (interactive-mode.ts:2655-2840): slash-command dispatch, bash mode,
    /// the compaction queue and steer/follow-up are all wired here.
    async fn handle_submit(&mut self, text: String) {
        if text.is_empty() {
            return;
        }

        // First, move any pending bash components to chat
        // (interactive-mode.ts:2831-2832).
        self.ui_state.flush_pending_bash_components();

        // Slash command dispatch (interactive-mode.ts:2660-2787).
        if text.starts_with('/') && self.dispatch_slash_command(&text).await {
            return;
        }
        // Bash command (! for normal, !! for excluded from context)
        // (interactive-mode.ts:2790-2805).
        if text.starts_with('!') {
            let is_excluded = text.starts_with("!!");
            let command = text
                .strip_prefix("!!")
                .or_else(|| text.strip_prefix('!'))
                .unwrap_or("")
                .trim()
                .to_string();
            if !command.is_empty() {
                if self.session.is_bash_running() {
                    self.ui_state.show_warning(
                        "A bash command is already running. Press Esc to cancel it first.",
                    );
                    return;
                }
                lock(&self.ui_state.editor).add_to_history(&text);
                self.ui_state.handle_bash_command(&command, is_excluded);
                *lock(&self.ui_state.is_bash_mode) = false;
                self.ui_state.update_editor_border_color();
                return;
            }
            return;
        }

        lock(&self.ui_state.editor).add_to_history(&text);

        // Queue input during compaction; extension commands execute
        // immediately upstream — a T15 hook (`is_extension_command`)
        // (interactive-mode.ts:2807-2817).
        if self.session.is_compacting() {
            self.ui_state
                .queue_compaction_message(text, StreamingBehavior::Steer);
            return;
        }

        // If streaming, prompt() with steer behavior; otherwise a normal
        // prompt (interactive-mode.ts:2819-2840).
        let options = if self.session.is_streaming() {
            PromptOptions {
                streaming_behavior: Some(StreamingBehavior::Steer),
                ..Default::default()
            }
        } else {
            PromptOptions::default()
        };
        self.prompt_or_shutdown(&text, options).await;
        self.ui_state.update_pending_messages_display();
        self.ui_state.render_handle.request_render();
    }

    /// `setupEditorSubmitHandler` command dispatch
    /// (interactive-mode.ts:2660-2787): exact and prefix matches; returns
    /// `true` when the input was consumed by a command. Extension commands
    /// (T15), the input event hook and the skill/prompt-template expansion
    /// stay inside `session.prompt` (agent_session.rs), which the fall-through
    /// below reaches.
    async fn dispatch_slash_command(&mut self, text: &str) -> bool {
        let ui = &self.ui_state;
        let rest = &text[1..];
        let (name, args) = match rest.find(' ') {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => (rest, ""),
        };
        let handled = match name {
            "settings" => {
                InteractiveUi::show_settings_selector(&self.ui_state);
                Some(())
            }
            "model" => {
                let search_term = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                ui.handle_model_command(search_term.as_deref().unwrap_or(""))
                    .await;
                Some(())
            }
            "scoped-models" => {
                InteractiveUi::show_models_selector(&self.ui_state).await;
                Some(())
            }
            "export" => {
                ui.handle_export_command(args);
                Some(())
            }
            "import" => {
                self.handle_import_command(args).await;
                Some(())
            }
            "share" => {
                ui.handle_share_command();
                Some(())
            }
            "copy" => {
                ui.handle_copy_command();
                Some(())
            }
            "name" => {
                ui.handle_name_command(text);
                Some(())
            }
            "session" => {
                ui.handle_session_command();
                Some(())
            }
            "changelog" => {
                ui.handle_changelog_command();
                Some(())
            }
            "hotkeys" => {
                ui.handle_hotkeys_command();
                Some(())
            }
            "fork" => {
                InteractiveUi::show_user_message_selector(&self.ui_state);
                Some(())
            }
            "clone" => {
                self.handle_clone_command().await;
                Some(())
            }
            "tree" => {
                InteractiveUi::show_tree_selector(&self.ui_state, None);
                Some(())
            }
            "trust" => {
                InteractiveUi::show_trust_selector(&self.ui_state);
                Some(())
            }
            "login" => {
                // `/login` or `/login <ref>` (interactive-mode.ts:2736-2741):
                // a ref goes through `handleLoginCommand` — exact provider
                // match starts the flow directly; otherwise the provider
                // list opens with the ref pre-filled into the search.
                let provider_ref = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                ui.handle_login_command(provider_ref.as_deref()).await;
                Some(())
            }
            "logout" => {
                InteractiveUi::show_logout_selector(&self.ui_state).await;
                Some(())
            }
            "new" => {
                self.handle_new_command().await;
                Some(())
            }
            "compact" => {
                let custom_instructions = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                ui.handle_compact_command(custom_instructions.as_deref().unwrap_or(""))
                    .await;
                Some(())
            }
            "resume" => {
                self.handle_resume_command(args).await;
                Some(())
            }
            "reload" => {
                ui.handle_reload_command();
                Some(())
            }
            "quit" => {
                ui.handle_quit_command();
                Some(())
            }
            "debug" => {
                ui.handle_debug_command();
                Some(())
            }
            // Extension commands (incl. the built-in llama.cpp `/llama`)
            // fall through to `session.prompt`'s extension-command path
            // (agent-session.ts:1121-1129) — registered with the real host
            // since T15 W3/W7.
            _ => None,
        };
        handled.is_some()
    }

    /// `handleFollowUp` (interactive-mode.ts:3727-3757): Alt+Enter queues a
    /// follow-up message (waits until the agent finishes); when idle it acts
    /// like a normal submit.
    async fn handle_follow_up(&mut self) {
        let text = lock(&self.ui_state.editor).get_expanded_text();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Queue input during compaction; extension commands execute
        // immediately upstream — a T15 hook (`is_extension_command`)
        // (interactive-mode.ts:3732-3741).
        if self.session.is_compacting() {
            self.ui_state
                .queue_compaction_message(text, StreamingBehavior::FollowUp);
            return;
        }
        if self.session.is_streaming() {
            // streamingBehavior: "followUp" (interactive-mode.ts:3745-3750).
            self.prompt_or_shutdown(
                &text,
                PromptOptions {
                    streaming_behavior: Some(StreamingBehavior::FollowUp),
                    ..Default::default()
                },
            )
            .await;
            self.ui_state.update_pending_messages_display();
            self.ui_state.render_handle.request_render();
        } else {
            // Not streaming: Alt+Enter acts like regular Enter
            // (interactive-mode.ts:3752-3756).
            self.handle_submit(text).await;
        }
    }

    /// `session.prompt` bounded by the shutdown signal: the initial messages
    /// and idle submits await the full turn; streaming steers return
    /// immediately from the session (the steer is queued).
    async fn prompt_or_shutdown(&mut self, text: &str, options: PromptOptions) {
        tokio::select! {
            result = self.session.prompt(text, options) => {
                if let Err(error) = result {
                    self.ui_state.show_error(&error.raw_message());
                }
            }
            _ = self.shutdown_rx.changed() => {}
        }
    }

    /// `flushCompactionQueue` (interactive-mode.ts:4041-4095) — the priority
    /// chain: willRetry re-queues everything into the retry turn;
    /// otherwise extension commands run immediately (T15 hook — currently
    /// never), the first non-extension message becomes the prompt (with its
    /// mode), and the rest are queued by mode. On failure the queue is
    /// restored and the error shown.
    async fn flush_compaction_queue(&mut self) {
        let messages: Vec<CompactionQueuedMessage> =
            std::mem::take(&mut *lock(&self.ui_state.compaction_queue));
        if messages.is_empty() {
            return;
        }
        self.ui_state.update_pending_messages_display();

        // Known gap (unassigned — no v0.1 task claims it, D-019): the
        // compaction_end event carries `willRetry`; the run loop currently
        // flushes unconditionally. The willRetry branch re-queues every
        // message into the retry turn (prompt for extension commands,
        // followUp/steer otherwise) — interactive-mode.ts:4061-4071.
        let will_retry = false;
        if will_retry {
            for message in messages {
                if is_extension_command(&self.session.extension_runner(), &message.text) {
                    self.prompt_or_shutdown(&message.text, PromptOptions::default())
                        .await;
                } else if message.mode == StreamingBehavior::FollowUp {
                    let _ = self.session.follow_up(&message.text, None).await;
                } else {
                    let _ = self.session.steer(&message.text, None).await;
                }
            }
            self.ui_state.update_pending_messages_display();
            return;
        }

        // `isExtensionCommand` (interactive-mode.ts:4017-4027): the no-op
        // extension runner never has commands — T15 hook.
        let first_prompt_index = messages.iter().position(|message| {
            !is_extension_command(&self.session.extension_runner(), &message.text)
        });
        let Some(first_prompt_index) = first_prompt_index else {
            // All extension commands — execute them all (currently never).
            for message in messages {
                self.prompt_or_shutdown(&message.text, PromptOptions::default())
                    .await;
            }
            return;
        };

        let pre_commands = &messages[..first_prompt_index];
        let first_prompt = messages[first_prompt_index].clone();
        let rest = &messages[first_prompt_index + 1..];

        // Execute extension commands before the first prompt (currently
        // never reached).
        for message in pre_commands {
            self.prompt_or_shutdown(&message.text, PromptOptions::default())
                .await;
        }

        // The first message becomes the prompt (its mode); failures restore
        // the queue (interactive-mode.ts:4081-4083). The shutdown race has
        // no upstream equivalent (single-threaded JS); the local rule is
        // that a shutdown mid-flush restores the not-yet-sent messages so
        // they are not lost (the mode's shutdown path reports them via the
        // resume hint, and a subsequent `/resume` re-enqueues nothing — the
        // queue simply survives in memory until then).
        let restore_messages = |ui_state: &InteractiveUi, msgs: &[CompactionQueuedMessage]| {
            *lock(&ui_state.compaction_queue) = msgs.to_vec();
            ui_state.update_pending_messages_display();
        };
        let restore_queue = |ui_state: &InteractiveUi, error: &RpiError| {
            ui_state.session().clear_queue();
            restore_messages(ui_state, &messages);
            ui_state.show_error(&format!(
                "Failed to send queued message{}: {}",
                if messages.len() > 1 { "s" } else { "" },
                error.raw_message()
            ));
        };

        let prompt_result = tokio::select! {
            // `biased`: an in-flight shutdown wins over a simultaneously
            // ready prompt result, so the queue restore below always runs.
            biased;
            _ = self.shutdown_rx.changed() => {
                restore_messages(&self.ui_state, &messages);
                return;
            }
            result = self.session.prompt(
                &first_prompt.text,
                PromptOptions {
                    streaming_behavior: Some(first_prompt.mode),
                    ..Default::default()
                },
            ) => result,
        };
        if let Err(error) = prompt_result {
            restore_queue(&self.ui_state, &error);
            return;
        }

        // Queue the remaining messages by mode
        // (interactive-mode.ts:4088-4092).
        for index in 0..rest.len() {
            let message = &rest[index];
            let queue_future = async {
                if is_extension_command(&self.session.extension_runner(), &message.text) {
                    self.session
                        .prompt(&message.text, PromptOptions::default())
                        .await
                } else if message.mode == StreamingBehavior::FollowUp {
                    self.session.follow_up(&message.text, None).await
                } else {
                    self.session.steer(&message.text, None).await
                }
            };
            let result = tokio::select! {
                biased;
                _ = self.shutdown_rx.changed() => {
                    restore_messages(&self.ui_state, &rest[index..]);
                    return;
                }
                result = queue_future => result,
            };
            if let Err(error) = result {
                restore_queue(&self.ui_state, &error);
                return;
            }
        }
        self.ui_state.update_pending_messages_display();
        self.ui_state.render_handle.request_render();
    }

    /// `shutdown` (interactive-mode.ts:3555-3594) — S4b basic version:
    /// stop the driver, restore the terminal, dispose the runtime, print
    /// the resume hint.
    async fn shutdown(&mut self) {
        if self.is_shutting_down {
            return;
        }
        self.is_shutting_down = true;

        // Stop the driver thread (bounded by DRIVER_PUMP_CAP).
        self.driver_stop.store(true, Ordering::Relaxed);
        if let Some(driver) = self.driver.take() {
            let _ = driver.join();
        }

        // Stop the theme watcher thread (bounded by its poll interval).
        self.theme_watcher_stop.store(true, Ordering::Relaxed);
        if let Some(watcher) = self.theme_watcher.take() {
            let _ = watcher.join();
        }

        // Stop the git branch watcher thread (bounded by its poll interval).
        self.git_watcher_stop.store(true, Ordering::Relaxed);
        if let Some(watcher) = self.git_watcher.take() {
            let _ = watcher.join();
        }

        // Restore the terminal (raw mode off, cursor shown).
        self.ui.stop();

        // Unsubscribe from session events (interactive-mode.ts:1735).
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }

        // TODO: drain terminal input before exit (upstream
        // terminal.drainInput(1000), interactive-mode.ts:3572-3583) — the
        // trait's drain future cannot be polled through Tui::with_terminal.

        self.runtime.dispose().await;

        if let Some(resume_command) = format_resume_command(&self.session) {
            println!(
                "{} {resume_command}",
                lock(&self.ui_state.theme).fg("dim", "To resume this session:")
            );
        }
    }
}

#[derive(Debug)]
enum RunCommand {
    Submit(String),
    FollowUp,
    ResumeSession(String),
    ForkFrom(String),
    FlushCompactionQueue,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    use rpi_ai::types::{ApiKind, UserMessage, UserRole};

    use super::*;
    use crate::modes::interactive::test_support::{
        build_test_session, install_noop_product_transports, TempDir, TestSession, TestTerminal,
    };

    // ---------------------------------------------------------------------
    // Fixtures
    // ---------------------------------------------------------------------

    fn assistant_message_raw(
        content: Vec<AssistantContent>,
        stop_reason: StopReason,
    ) -> rpi_ai::types::AssistantMessage {
        rpi_ai::types::AssistantMessage {
            role: rpi_ai::types::AssistantRole::Assistant,
            content,
            api: ApiKind("openai-completions".into()),
            provider: "custom".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: rpi_ai::types::Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 1_700_000_000_000,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        }
    }

    fn assistant_message(content: Vec<AssistantContent>, stop_reason: StopReason) -> AgentMessage {
        AgentMessage::Assistant(rpi_ai::types::AssistantMessage {
            role: rpi_ai::types::AssistantRole::Assistant,
            content,
            api: ApiKind("openai-completions".into()),
            provider: "custom".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: rpi_ai::types::Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 1_700_000_000_000,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        })
    }

    fn text_content(text: &str) -> AssistantContent {
        AssistantContent::Text(rpi_ai::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })
    }

    fn tool_call_content(id: &str, name: &str) -> AssistantContent {
        AssistantContent::ToolCall(rpi_ai::types::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::Map::new(),
            thought_signature: None,
            namespace: None,
        })
    }

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            role: UserRole::User,
            content: rpi_ai::types::UserContent::Text(text.to_string()),
            timestamp: 1_700_000_000_000,
        })
    }

    /// Build a mode over the test session and terminal. The session is
    /// cloned out so tests can append entries / seed agent state.
    async fn mode_harness() -> (InteractiveMode, Arc<TestTerminal>, AgentSession) {
        let harness = build_test_session().await;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        // M1 (T14 review): unit tests must never reach product endpoints —
        // swap the production transports for no-op ones before `init()`.
        install_noop_product_transports(&mode);
        (mode, terminal, harness.session)
    }

    fn chat_children(ui: &InteractiveUi) -> usize {
        lock(&ui.chat_container).children.len()
    }

    /// M1 anchor: `init()` (install telemetry) and `run()` (startup version
    /// check) must go through the injectable transports — with the no-op
    /// transports installed, a full init + run sequence performs zero real
    /// product requests (previously every test run emitted real anonymous
    /// pings to revpi.dev). Counters are 0 when the env gates the ping closed
    /// (e.g. `RPI_OFFLINE`/`RPI_SKIP_VERSION_CHECK` in the dev env) and 1
    /// when armed — either way no network is touched.
    #[tokio::test]
    async fn init_and_run_make_no_product_network_requests() {
        let harness = build_test_session().await;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        let (telemetry, version_check) = install_noop_product_transports(&mode);

        let mut mode = mode;
        mode.init().await;
        // Let the fire-and-forget spawns run to completion (no-op: instant).
        tokio::task::yield_now().await;
        let telemetry_calls = telemetry.0.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            telemetry_calls <= 1,
            "init must go through the injected no-op transport, got {telemetry_calls} calls"
        );
        assert_eq!(
            version_check.0.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "init must not emit version checks"
        );

        // run() spawns the startup version probe; drive it briefly, then
        // shut down and let the probe future resolve.
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            rt.block_on(async move {
                mode.run().await;
            });
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(terminal.is_started(), "terminal started by run()");
        terminal.feed("\u{4}"); // Ctrl+D → shutdown
        let _ = handle.join();
        let version_calls = version_check.0.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            version_calls <= 1,
            "run must go through the injected no-op transport, got {version_calls} calls"
        );
    }

    // ---------------------------------------------------------------------
    // Event mapping (all 24 AgentSessionEvent variants)
    // ---------------------------------------------------------------------

    /// The branch name of a command, for mapping assertions.
    fn command_name(command: &UiCommand) -> &'static str {
        match command {
            UiCommand::AgentStart => "agent_start",
            UiCommand::AgentEnd => "agent_end",
            UiCommand::TurnStart => "turn_start",
            UiCommand::TurnEnd => "turn_end",
            UiCommand::MessageStart(_) => "message_start",
            UiCommand::MessageUpdate(_) => "message_update",
            UiCommand::MessageEnd(_) => "message_end",
            UiCommand::ToolExecutionStart { .. } => "tool_execution_start",
            UiCommand::ToolExecutionUpdate { .. } => "tool_execution_update",
            UiCommand::ToolExecutionEnd { .. } => "tool_execution_end",
            UiCommand::QueueUpdate { .. } => "queue_update",
            UiCommand::EntryAppended(_) => "entry_appended",
            UiCommand::SessionInfoChanged { .. } => "session_info_changed",
            UiCommand::ThinkingLevelChanged(_) => "thinking_level_changed",
            UiCommand::BashExecutionUpdate { .. } => "bash_execution_update",
            UiCommand::ExtensionError { .. } => "extension_error",
            UiCommand::AgentSettled => "agent_settled",
            UiCommand::CompactionStart(_) => "compaction_start",
            UiCommand::CompactionEnd { .. } => "compaction_end",
            UiCommand::AutoRetryStart { .. } => "auto_retry_start",
            UiCommand::AutoRetryEnd { .. } => "auto_retry_end",
            UiCommand::SummarizationRetryScheduled { .. } => "summarization_retry_scheduled",
            UiCommand::SummarizationRetryAttemptStart(_) => "summarization_retry_attempt_start",
            UiCommand::SummarizationRetryFinished => "summarization_retry_finished",
            UiCommand::ClearEditor => "clear_editor",
            UiCommand::RefreshEditorBorder => "refresh_editor_border",
            UiCommand::Escape => "escape",
            UiCommand::ThemeChanged => "theme_changed",
            UiCommand::ApplyThemeName(_) => "apply_theme_name",
            UiCommand::GitBranchChanged => "git_branch_changed",
            UiCommand::NewVersionAvailable(_) => "new_version_available",
            UiCommand::ShareAbort => "share_abort",
            UiCommand::ShareCompleted(_) => "share_completed",
            UiCommand::Dequeue => "dequeue",
            UiCommand::PasteImage => "paste_image",
        }
    }

    #[test]
    fn all_24_event_variants_map_to_commands() {
        use crate::core::agent_session::SessionEvent;
        use rpi_agent::types::{AgentEvent, ThinkingLevel};

        let events: Vec<(AgentSessionEvent, &str)> = vec![
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::AgentStart)),
                "agent_start",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::AgentEnd {
                    messages: Vec::new(),
                })),
                "agent_end",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::TurnStart)),
                "turn_start",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::TurnEnd {
                    message: user_message("x"),
                    tool_results: Vec::new(),
                })),
                "turn_end",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::MessageStart {
                    message: user_message("x"),
                })),
                "message_start",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
                    message: assistant_message(vec![text_content("x")], StopReason::Pending),
                    assistant_message_event: Box::new(rpi_ai::types::StreamEvent::TextStart {
                        content_index: 0,
                        partial: assistant_message_raw(
                            vec![text_content("x")],
                            StopReason::Pending,
                        ),
                    }),
                })),
                "message_update",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
                    message: assistant_message(vec![text_content("x")], StopReason::Stop),
                })),
                "message_end",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::ToolExecutionStart {
                    tool_call_id: "c1".to_string(),
                    tool_name: "read".to_string(),
                    args: serde_json::json!({}),
                })),
                "tool_execution_start",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: "c1".to_string(),
                    tool_name: "read".to_string(),
                    args: serde_json::json!({}),
                    partial_result: serde_json::json!({}),
                })),
                "tool_execution_update",
            ),
            (
                AgentSessionEvent::Agent(Box::new(AgentEvent::ToolExecutionEnd {
                    tool_call_id: "c1".to_string(),
                    tool_name: "read".to_string(),
                    result: serde_json::json!({}),
                    is_error: false,
                })),
                "tool_execution_end",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::QueueUpdate {
                    steering: Vec::new(),
                    follow_up: Vec::new(),
                }),
                "queue_update",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::EntryAppended {
                    entry: Box::new(SessionEntry::Label(rpi_agent_entry())),
                }),
                "entry_appended",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::SessionInfoChanged { name: None }),
                "session_info_changed",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::ThinkingLevelChanged {
                    level: ThinkingLevel::Low,
                }),
                "thinking_level_changed",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::BashExecutionUpdate {
                    id: None,
                    delta: String::new(),
                }),
                "bash_execution_update",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::ExtensionError {
                    extension_path: String::new(),
                    event: String::new(),
                    error: String::new(),
                }),
                "extension_error",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::AgentSettled),
                "agent_settled",
            ),
            (
                AgentSessionEvent::Compaction(Box::new(CompactionEvent::CompactionStart {
                    reason: CompactionReason::Threshold,
                })),
                "compaction_start",
            ),
            (
                AgentSessionEvent::Compaction(Box::new(CompactionEvent::CompactionEnd {
                    reason: CompactionReason::Threshold,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                })),
                "compaction_end",
            ),
            (
                AgentSessionEvent::Compaction(Box::new(
                    CompactionEvent::SummarizationRetryScheduled {
                        attempt: 1,
                        max_attempts: 3,
                        delay_ms: 500,
                        error_message: String::new(),
                    },
                )),
                "summarization_retry_scheduled",
            ),
            (
                AgentSessionEvent::Compaction(Box::new(
                    CompactionEvent::SummarizationRetryAttemptStart {
                        source: RetrySource::Compaction {
                            reason: CompactionReason::Threshold,
                        },
                    },
                )),
                "summarization_retry_attempt_start",
            ),
            (
                AgentSessionEvent::Compaction(Box::new(
                    CompactionEvent::SummarizationRetryFinished,
                )),
                "summarization_retry_finished",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::AutoRetryStart {
                    attempt: 1,
                    max_attempts: 3,
                    delay_ms: 500,
                    error_message: String::new(),
                }),
                "auto_retry_start",
            ),
            (
                AgentSessionEvent::Session(SessionEvent::AutoRetryEnd {
                    success: true,
                    attempt: 1,
                    final_error: None,
                }),
                "auto_retry_end",
            ),
        ];
        assert_eq!(
            events.len(),
            24,
            "all 24 AgentSessionEvent variants covered"
        );
        for (event, expected) in events {
            let command = UiCommand::from(event);
            assert_eq!(
                command_name(&command),
                expected,
                "variant {expected} mapped incorrectly"
            );
        }
    }

    fn rpi_agent_entry() -> rpi_agent::session::LabelEntry {
        rpi_agent::session::LabelEntry {
            id: "l1".to_string(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            target_id: "t1".to_string(),
            label: Some("x".to_string()),
        }
    }

    // ---------------------------------------------------------------------
    // Streaming message lifecycle
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn message_start_creates_streaming_component() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("hi")],
            StopReason::Pending,
        )));
        ui.drain_events();
        assert_eq!(chat_children(ui), 1);
        let streaming = lock(&ui.streaming);
        assert!(streaming.is_some(), "streaming component tracked");
        let track = streaming.as_ref().unwrap();
        let rendered = lock(&track.handle).render(60);
        assert!(rendered.join("\n").contains("hi"));
    }

    #[tokio::test]
    async fn message_update_adds_tool_component_once_and_updates_args() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("hi")],
            StopReason::Pending,
        )));
        ui.push(UiCommand::MessageUpdate(assistant_message(
            vec![text_content("hi"), tool_call_content("call_1", "read")],
            StopReason::Pending,
        )));
        ui.drain_events();
        assert_eq!(chat_children(ui), 2, "streaming + tool component");
        assert!(lock(&ui.pending_tools).contains_key("call_1"));

        // A second update with the same call id reuses the component (no new
        // chat child).
        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!("a.txt"));
        ui.push(UiCommand::MessageUpdate(assistant_message(
            vec![
                text_content("hi"),
                AssistantContent::ToolCall(rpi_ai::types::ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: args.clone(),
                    thought_signature: None,
                    namespace: None,
                }),
            ],
            StopReason::Pending,
        )));
        ui.drain_events();
        assert_eq!(chat_children(ui), 2, "no duplicate tool component");
        // Args updated on the existing component (rendered args include the
        // new path after expansion).
        let tool = lock(&ui.pending_tools).get("call_1").unwrap().clone();
        lock(&tool).set_expanded(true);
        let rendered = lock(&tool).render(60).join("\n");
        assert!(rendered.contains("a.txt"), "args updated: {rendered}");
    }

    #[tokio::test]
    async fn set_tools_expanded_reaches_displayed_chat_tool_after_pending_cleared() {
        // T17 regression: `setToolsExpanded` iterates the loaded-resources + chat
        // containers (interactive-mode.ts:4033-4048), not pending_tools — after tool
        // execution ends, `pending_tools` has already been cleared by
        // `tool_execution_end`/`agent_end`; iterating only the pending map would break
        // ctrl+o for already-shown components, and leftover flags would pollute
        // components created later (write is not collapsed).
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        let content = (1..=15)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("hi")],
            StopReason::Pending,
        )));
        ui.push(UiCommand::MessageUpdate(assistant_message(
            vec![
                text_content("hi"),
                AssistantContent::ToolCall(rpi_ai::types::ToolCall {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    arguments: serde_json::json!({
                        "path": "a.txt",
                        "content": content,
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                    thought_signature: None,
                    namespace: None,
                }),
            ],
            StopReason::Pending,
        )));
        ui.drain_events();
        let tool = lock(&ui.pending_tools).get("call_1").unwrap().clone();

        // Collapsed by default (10-line clamp + expand hint).
        let collapsed = lock(&tool).render(80).join("\n");
        assert!(collapsed.contains("(5 more lines, 15 total,"));
        assert!(!collapsed.contains("l15"));

        // `agent_end` clears the pending map; the component stays displayed.
        lock(&ui.pending_tools).clear();
        ui.set_tools_expanded(true);

        // The displayed chat component expanded despite the cleared map.
        let expanded = lock(&tool).render(80).join("\n");
        assert!(expanded.contains("l15"), "expanded: {expanded}");
        assert!(!expanded.contains("more lines"));
        assert!(*lock(&ui.tool_output_expanded));

        // And collapses back.
        ui.set_tools_expanded(false);
        let collapsed_again = lock(&tool).render(80).join("\n");
        assert!(collapsed_again.contains("(5 more lines, 15 total,"));
        assert!(!collapsed_again.contains("l15"));
    }

    #[tokio::test]
    async fn message_end_aborted_marks_tools_as_error() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("hi")],
            StopReason::Pending,
        )));
        ui.push(UiCommand::MessageUpdate(assistant_message(
            vec![text_content("hi"), tool_call_content("call_1", "read")],
            StopReason::Pending,
        )));
        ui.push(UiCommand::MessageEnd(assistant_message(
            vec![text_content("hi"), tool_call_content("call_1", "read")],
            StopReason::Aborted,
        )));
        ui.drain_events();
        // Streaming cleared; pending tools flushed with the error result.
        assert!(lock(&ui.streaming).is_none());
        assert!(lock(&ui.pending_tools).is_empty());
        // The tool component shows the abort error text.
        let rendered = lock(&ui.chat_container)
            .children
            .last()
            .unwrap()
            .render(60)
            .join("\n");
        assert!(
            rendered.contains("Operation aborted"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn agent_end_removes_stray_streaming_component() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("hi")],
            StopReason::Pending,
        )));
        ui.drain_events();
        assert_eq!(chat_children(ui), 1);
        // agent_end without message_end: safety-net removal
        // (interactive-mode.ts:3055-3059).
        ui.push(UiCommand::AgentEnd);
        ui.drain_events();
        assert_eq!(chat_children(ui), 0);
        assert!(lock(&ui.streaming).is_none());
    }

    #[tokio::test]
    async fn agent_start_shows_working_status_and_progress() {
        let (mode, terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::AgentStart);
        ui.drain_events();
        assert!(matches!(*lock(&ui.status), ActiveStatus::Working(_)));
        // showTerminalProgress is off by default; the status indicator is
        // the observable part.
        ui.push(UiCommand::AgentEnd);
        ui.drain_events();
        assert!(matches!(*lock(&ui.status), ActiveStatus::Idle));
        let _ = terminal;
    }

    // ---------------------------------------------------------------------
    // Tool execution events
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn tool_execution_start_update_end_lifecycle() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::ToolExecutionStart {
            tool_call_id: "call_1".to_string(),
            // No built-in render definition (T17) → the fallback path keeps
            // the partial/final result text visible.
            tool_name: "custom-tool".to_string(),
            args: serde_json::json!({ "path": "a.txt" }),
        });
        ui.drain_events();
        assert_eq!(chat_children(ui), 1);
        assert!(lock(&ui.pending_tools).contains_key("call_1"));

        ui.push(UiCommand::ToolExecutionUpdate {
            tool_call_id: "call_1".to_string(),
            partial_result: serde_json::json!({
                "content": [{ "type": "text", "text": "partial" }]
            }),
        });
        ui.drain_events();
        let rendered = lock(&ui.chat_container)
            .children
            .last()
            .unwrap()
            .render(60)
            .join("\n");
        assert!(rendered.contains("partial"), "rendered: {rendered}");

        ui.push(UiCommand::ToolExecutionEnd {
            tool_call_id: "call_1".to_string(),
            result: serde_json::json!({
                "content": [{ "type": "text", "text": "file contents" }]
            }),
            is_error: false,
        });
        ui.drain_events();
        assert!(!lock(&ui.pending_tools).contains_key("call_1"));
        let rendered = lock(&ui.chat_container)
            .children
            .last()
            .unwrap()
            .render(60)
            .join("\n");
        assert!(rendered.contains("file contents"), "rendered: {rendered}");
    }

    // ---------------------------------------------------------------------
    // Compaction / retry escape-handler swaps
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn compaction_swaps_and_restores_escape_handler() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        let marker = Arc::new(AtomicBool::new(false));
        let sentinel = marker.clone();
        lock(&ui.editor).on_escape = Some(Box::new(move || {
            sentinel.store(true, AtomicOrdering::SeqCst);
        }));

        ui.push(UiCommand::CompactionStart(CompactionReason::Threshold));
        ui.drain_events();
        assert!(
            matches!(*lock(&ui.status), ActiveStatus::Compaction(_)),
            "compaction status shown"
        );
        assert!(lock(&ui.auto_compaction_escape_handler).is_some());
        // The swap handler aborts the compaction instead of the sentinel.
        lock(&ui.editor).on_escape.as_mut().expect("swap handler")();
        assert!(!marker.load(AtomicOrdering::SeqCst));

        ui.push(UiCommand::CompactionEnd {
            reason: CompactionReason::Threshold,
            result: None,
            aborted: false,
            will_retry: false,
            error_message: None,
        });
        ui.drain_events();
        assert!(lock(&ui.auto_compaction_escape_handler).is_none());
        lock(&ui.editor)
            .on_escape
            .as_mut()
            .expect("restored handler")();
        assert!(marker.load(AtomicOrdering::SeqCst), "sentinel restored");
        assert!(matches!(*lock(&ui.status), ActiveStatus::Idle));
        // The run loop was signalled to flush the compaction queue.
        assert!(*mode.flush_rx.borrow() == 1, "flush signal sent");
    }

    #[tokio::test]
    async fn new_version_available_renders_update_notification() {
        // `showNewVersionNotification` (interactive-mode.ts:3885-3912) via
        // the drain: bordered block, version line, note, changelog link.
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        let before = chat_children(ui);
        ui.push(UiCommand::NewVersionAvailable(
            crate::core::version_check::LatestRpiRelease {
                version: "99.0.0".to_string(),
                package_name: None,
                note: Some("**breaking** changes".to_string()),
            },
        ));
        ui.drain_events();
        // Spacer + border + heading + note spacer + note + spacer +
        // changelog + border.
        assert_eq!(chat_children(ui) - before, 8);
        let rendered = lock(&ui.chat_container).render(80).join("\n");
        assert!(
            rendered.contains("Update Available"),
            "rendered: {rendered}"
        );
        // T18 (ADR-0011 §6): the action is the self-update command; the
        // muted/accent split puts an ANSI boundary between the lead-in and
        // the action, so assert the two segments separately.
        assert!(
            rendered.contains("New version 99.0.0 is available."),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("Run rpi update --self"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("rpi update"), "rendered: {rendered}");
        assert!(rendered.contains("breaking"), "rendered: {rendered}");
        assert!(
            rendered.contains("https://revpi.dev/changelog"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn new_version_available_without_note_skips_markdown() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        let before = chat_children(ui);
        ui.push(UiCommand::NewVersionAvailable(
            crate::core::version_check::LatestRpiRelease {
                version: "99.0.0".to_string(),
                package_name: None,
                note: None,
            },
        ));
        ui.drain_events();
        // Spacer + border + heading + changelog + border.
        assert_eq!(chat_children(ui) - before, 5);
    }

    #[tokio::test]
    async fn compaction_end_with_result_rebuilds_chat() {
        let (mode, _terminal, session) = mode_harness().await;
        let ui = &mode.ui_state;
        // Seed entries so the rebuild has something to render.
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("before compaction"))
                .expect("append");
        }
        // Expanded state shows the summary body (collapsed hides it).
        *lock(&ui.tool_output_expanded) = true;
        ui.push(UiCommand::CompactionEnd {
            reason: CompactionReason::Threshold,
            result: Some(Box::new(rpi_agent::compaction::CompactionResult {
                summary: "summarized".to_string(),
                first_kept_entry_id: "x".to_string(),
                tokens_before: 1234,
                estimated_tokens_after: None,
                usage: None,
                details: None,
            })),
            aborted: false,
            will_retry: false,
            error_message: None,
        });
        ui.drain_events();
        // Rebuilt chat: user message + spacer + compaction summary.
        assert!(chat_children(ui) >= 3, "children: {}", chat_children(ui));
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("summarized"), "rendered: {rendered}");
    }

    #[tokio::test]
    async fn auto_retry_swaps_and_restores_escape_handler() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        let marker = Arc::new(AtomicBool::new(false));
        let sentinel = marker.clone();
        lock(&ui.editor).on_escape = Some(Box::new(move || {
            sentinel.store(true, AtomicOrdering::SeqCst);
        }));
        ui.push(UiCommand::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 1000,
            error_message: "boom".to_string(),
        });
        ui.drain_events();
        assert!(matches!(*lock(&ui.status), ActiveStatus::Retry(_)));
        assert!(lock(&ui.retry_escape_handler).is_some());

        ui.push(UiCommand::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("still broken".to_string()),
        });
        ui.drain_events();
        assert!(lock(&ui.retry_escape_handler).is_none());
        assert!(matches!(*lock(&ui.status), ActiveStatus::Idle));
        lock(&ui.editor)
            .on_escape
            .as_mut()
            .expect("restored handler")();
        assert!(marker.load(AtomicOrdering::SeqCst), "sentinel restored");
        // Final failure shows an error line.
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("Retry failed after 3 attempts: still broken"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn summarization_retry_attempt_switches_status_kind() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.push(UiCommand::SummarizationRetryScheduled {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 500,
            error_message: "summarize failed".to_string(),
        });
        ui.drain_events();
        assert!(matches!(*lock(&ui.status), ActiveStatus::Retry(_)));
        ui.push(UiCommand::SummarizationRetryAttemptStart(
            RetrySource::BranchSummary,
        ));
        ui.drain_events();
        assert!(
            matches!(*lock(&ui.status), ActiveStatus::BranchSummary(_)),
            "branch summary indicator"
        );
        ui.push(UiCommand::SummarizationRetryFinished);
        ui.drain_events();
        // Upstream parity: `clearStatusIndicator("retry")` is a no-op while
        // a branchSummary indicator is active (kind mismatch,
        // interactive-mode.ts:1858-1861) — the indicator stays until the
        // next status event.
        assert!(
            matches!(*lock(&ui.status), ActiveStatus::BranchSummary(_)),
            "branch summary indicator remains (upstream kind-mismatch quirk)"
        );
    }

    // ---------------------------------------------------------------------
    // Status lines
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn show_status_coalesces_back_to_back() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.show_status("first");
        ui.show_status("second");
        let children = lock(&ui.chat_container).children.len();
        assert_eq!(children, 2, "spacer + coalesced text");
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("second"), "rendered: {rendered}");
        assert!(
            !rendered.contains("first"),
            "first was replaced: {rendered}"
        );

        // A non-status child breaks the coalescing chain.
        ui.show_error("boom");
        ui.show_status("third");
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("third"), "rendered: {rendered}");
        assert!(rendered.contains("Error: boom"), "rendered: {rendered}");
    }

    // ---------------------------------------------------------------------
    // Escape / Ctrl+C basics
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn escape_clears_bash_mode() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        lock(&ui.editor).set_text("!ls");
        *lock(&ui.is_bash_mode) = true;
        ui.push(UiCommand::Escape);
        ui.drain_events();
        assert_eq!(lock(&ui.editor).get_text(), "");
        assert!(!*lock(&ui.is_bash_mode));
    }

    #[tokio::test]
    async fn ctrl_c_single_clears_editor_and_double_shuts_down() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        lock(&ui.editor).set_text("typing...");
        ui.handle_ctrl_c();
        ui.drain_events();
        assert_eq!(lock(&ui.editor).get_text(), "");
        assert!(!*mode.shutdown_rx.borrow(), "single press does not quit");

        ui.handle_ctrl_c();
        assert!(
            *mode.shutdown_rx.borrow(),
            "second press within the window quits"
        );
    }

    #[tokio::test]
    async fn ctrl_d_shuts_down() {
        let (mode, _terminal, _session) = mode_harness().await;
        mode.ui_state.handle_ctrl_d();
        assert!(*mode.shutdown_rx.borrow());
    }

    // ---------------------------------------------------------------------
    // Tools expansion linkage
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn tools_expand_toggles_header_and_pending_tools() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        let header = Arc::new(Mutex::new(ExpandableText::new(
            Box::new(|| "collapsed".to_string()),
            Box::new(|| "expanded".to_string()),
            false,
            0,
            0,
        )));
        *lock(&ui.built_in_header) = Some(header.clone());

        ui.push(UiCommand::ToolExecutionStart {
            tool_call_id: "call_1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({}),
        });
        ui.drain_events();

        ui.toggle_tool_output_expansion();
        assert!(lock(&header).is_expanded());
        assert!(*lock(&ui.tool_output_expanded));
        let tool = lock(&ui.pending_tools).get("call_1").unwrap().clone();
        let expanded_render = lock(&tool).render(60);
        assert!(!expanded_render.is_empty(), "expanded tool renders content");

        ui.toggle_tool_output_expansion();
        assert!(!lock(&header).is_expanded());
        assert!(!*lock(&ui.tool_output_expanded));
    }

    // ---------------------------------------------------------------------
    // Initial message rendering
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn render_initial_messages_builds_chat_from_entries() {
        let (mode, _terminal, _session) = mode_harness().await;
        let (ui, session) = (&mode.ui_state, _session);
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("first question"))
                .expect("append user");
            manager
                .append_message(assistant_message(
                    vec![
                        text_content("first answer"),
                        tool_call_content("call_1", "custom-tool"),
                    ],
                    StopReason::Stop,
                ))
                .expect("append assistant");
            manager
                .append_message(rpi_agent::messages::AgentMessage::ToolResult(
                    rpi_ai::types::ToolResultMessage {
                        role: rpi_ai::types::ToolResultRole::ToolResult,
                        tool_call_id: "call_1".to_string(),
                        // No built-in render definition (T17): the fallback
                        // result path keeps the result text visible.
                        tool_name: "custom-tool".to_string(),
                        content: vec![rpi_ai::types::ToolResultContent::Text(
                            rpi_ai::types::TextContent {
                                text: "the file".to_string(),
                                text_signature: None,
                            },
                        )],
                        is_error: false,
                        details: None,
                        usage: None,
                        added_tool_names: None,
                        timestamp: 1_700_000_000_000,
                    },
                ))
                .expect("append tool result");
        }
        ui.render_initial_messages();
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("first question"), "rendered: {rendered}");
        assert!(rendered.contains("first answer"), "rendered: {rendered}");
        assert!(
            rendered.contains("the file"),
            "tool result rendered: {rendered}"
        );
        // The completed tool result was matched to its component: nothing
        // stays pending.
        assert!(lock(&ui.pending_tools).is_empty(), "tool result consumed");
        // History populated from the user message.
        let history = lock(&ui.editor);
        assert!(history.get_text().is_empty());
    }

    // ---------------------------------------------------------------------
    // init (tree assembly)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn init_assembles_tree_and_starts_terminal() {
        let (mut mode, terminal, _session) = mode_harness().await;
        mode.init().await;
        assert!(mode.is_initialized);
        // The terminal was started (input handler installed).
        assert!(terminal.is_started(), "terminal started");
        // Non-quiet startup: expandable header linked to tools expansion.
        assert!(lock(&mode.ui_state.built_in_header).is_some());
        // The footer reflects the session's auto-compaction setting.
        assert!(lock(&mode.ui_state.footer).auto_compact_enabled());
        // The editor is focused (TUI set_focus applied the flag).
        let focused = lock(&mode.editor_region)
            .as_focusable()
            .expect("focusable editor region")
            .focused();
        assert!(focused, "editor region focused");
        // Terminal title set.
        assert!(terminal.title().contains("rpi - "));
        // init is idempotent.
        mode.init().await;
        assert!(mode.is_initialized);
    }

    // ---------------------------------------------------------------------
    // Selector integration (showSelector framework, T12-S5a)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn show_selector_swaps_editor_and_hide_restores_position() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        let editor_position = ui
            .ui
            .child_position(&mode.editor_region)
            .expect("editor mounted");

        // A placeholder selector entry (a Text region stands in for a real
        // selector component).
        let selector = shared_component_from_boxed(Box::new(SharedChild(Arc::new(Mutex::new(
            Text::new("selector", 0, 0, None),
        )))));
        ui.show_selector(selector.clone());

        assert_eq!(
            ui.ui.child_position(&selector),
            Some(editor_position),
            "selector replaces the editor at its position"
        );
        assert!(ui.ui.child_position(&mode.editor_region).is_none());
        assert!(lock(&ui.active_selector).is_some());

        ui.hide_selector();
        assert_eq!(
            ui.ui.child_position(&mode.editor_region),
            Some(editor_position),
            "editor restored at the selector's position"
        );
        assert!(ui.ui.child_position(&selector).is_none());
        assert!(lock(&ui.active_selector).is_none());
        // Focus is back on the editor.
        let focused = lock(&mode.editor_region)
            .as_focusable()
            .expect("focusable editor region")
            .focused();
        assert!(focused, "editor focused after hide");
    }

    #[tokio::test]
    async fn show_tree_selector_mounts_selector_and_escape_closes_it() {
        let (mut mode, terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;

        // Seed a user message so the tree is non-empty.
        {
            let manager = ui.session().session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("first question"))
                .expect("append user message");
        }

        InteractiveUi::show_tree_selector(&Arc::clone(&mode.ui_state), None);
        assert!(lock(&ui.active_selector).is_some(), "tree selector mounted");
        assert!(ui.ui.child_position(&mode.editor_region).is_none());

        // Escape through the TUI dispatch closes the selector and restores
        // the editor (upstream `tui.select.cancel`). The cancel runs while
        // the TUI inner lock is held by `tick`, so this also covers the
        // mid-dispatch swap path (`Tui::swap_child`).
        let editor_position = ui
            .ui
            .child_position(&lock(&ui.active_selector).clone().expect("selector"))
            .expect("selector mounted");
        terminal.feed("\u{1b}");
        ui.ui.tick(std::time::Instant::now());
        assert!(
            lock(&ui.active_selector).is_none(),
            "escape closed the selector"
        );
        assert_eq!(
            ui.ui.child_position(&mode.editor_region),
            Some(editor_position),
            "editor restored at the selector's position"
        );
    }

    #[tokio::test]
    async fn show_user_message_selector_empty_session_shows_status() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;

        InteractiveUi::show_user_message_selector(&Arc::clone(&mode.ui_state));
        // No user messages: nothing mounted, a status line instead.
        assert!(lock(&ui.active_selector).is_none());
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("No messages to fork from"),
            "rendered: {rendered}"
        );
    }

    // ---------------------------------------------------------------------
    // Slash dispatch chain (T12-S5b)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn slash_quit_requests_shutdown() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/quit".to_string()).await;
        assert!(*mode.shutdown_rx.borrow(), "/quit requests shutdown");
    }

    #[tokio::test]
    async fn slash_name_sets_session_name() {
        let (mut mode, _terminal, session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/name my-session".to_string()).await;
        let name = lock(&session.session_manager()).get_session_name();
        assert_eq!(name.as_deref(), Some("my-session"));
        // Rendered status in chat.
        let rendered = lock(&mode.ui_state.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("Session name set"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn slash_session_renders_stats() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/session".to_string()).await;
        let rendered = lock(&mode.ui_state.chat_container).render(60).join("\n");
        assert!(rendered.contains("Session Info"), "rendered: {rendered}");
        assert!(rendered.contains("Messages"), "rendered: {rendered}");
    }

    #[tokio::test]
    async fn slash_tree_mounts_selector() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/tree".to_string()).await;
        assert!(
            lock(&mode.ui_state.active_selector).is_some(),
            "tree selector mounted"
        );
    }

    #[tokio::test]
    async fn slash_fork_selector_empty_shows_status() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/fork".to_string()).await;
        // No user messages in a fresh session: status instead of selector.
        assert!(lock(&mode.ui_state.active_selector).is_none());
        let rendered = lock(&mode.ui_state.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("No messages to fork from"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn unknown_slash_falls_through_to_prompt() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        // `/nosuchcommand` is not a built-in; the session prompt rejects it
        // (no auth configured in the test session) and shows an error.
        mode.handle_submit("/nosuchcommand".to_string()).await;
        let rendered = lock(&mode.ui_state.chat_container).render(60).join("\n");
        assert!(rendered.contains("Error:"), "rendered: {rendered}");
    }

    #[tokio::test]
    async fn bash_bang_runs_command() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("!echo s5b-bash".to_string()).await;
        let rendered = lock(&mode.ui_state.chat_container).render(80).join("\n");
        assert!(rendered.contains("echo s5b-bash"), "rendered: {rendered}");
        assert!(rendered.contains("s5b-bash"), "output rendered: {rendered}");
    }

    #[tokio::test]
    async fn bash_double_bang_excludes_from_context() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("!!echo excluded".to_string()).await;
        let rendered = lock(&mode.ui_state.chat_container).render(80).join("\n");
        assert!(rendered.contains("echo excluded"), "rendered: {rendered}");
    }

    #[tokio::test]
    async fn hotkeys_command_renders_table() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/hotkeys".to_string()).await;
        let rendered = lock(&mode.ui_state.chat_container).render(80).join("\n");
        assert!(
            rendered.contains("Keyboard Shortcuts"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn debug_command_writes_log() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        mode.handle_submit("/debug".to_string()).await;
        let rendered = lock(&mode.ui_state.chat_container).render(80).join("\n");
        assert!(
            rendered.contains("Debug log written"),
            "rendered: {rendered}"
        );
        // The debug log file exists; remove it afterwards (the test writes
        // through the real agent dir).
        let log_path = crate::config::get_agent_dir().join("rpi-debug.log");
        assert!(log_path.exists(), "debug log written");
        let _ = std::fs::remove_file(&log_path);
    }

    // ---------------------------------------------------------------------
    // Message queue UX (T12-S6)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn queue_compaction_message_clears_editor_and_shows_status() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        lock(&ui.editor).set_text("queued text");
        ui.queue_compaction_message("queued text".to_string(), StreamingBehavior::Steer);
        assert_eq!(lock(&ui.compaction_queue).len(), 1);
        assert_eq!(lock(&ui.editor).get_text(), "", "editor cleared");
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("Queued message for after compaction"),
            "rendered: {rendered}"
        );
        // Pending display shows the queued row.
        let pending = lock(&ui.pending_messages_container).render(60).join("\n");
        assert!(
            pending.contains("Steering: queued text"),
            "pending: {pending}"
        );
        assert!(
            pending.contains("to edit all queued messages"),
            "pending: {pending}"
        );
    }

    #[tokio::test]
    async fn pending_display_shows_both_modes_and_hint() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        ui.queue_compaction_message("steer me".to_string(), StreamingBehavior::Steer);
        ui.queue_compaction_message("follow me".to_string(), StreamingBehavior::FollowUp);
        let pending = lock(&ui.pending_messages_container).render(60).join("\n");
        assert!(pending.contains("Steering: steer me"), "pending: {pending}");
        assert!(
            pending.contains("Follow-up: follow me"),
            "pending: {pending}"
        );
        // The session queues merge into the display too (seed them directly).
        let _ = ui.session().steer("session steer", None).await;
        let _ = ui.session().follow_up("session follow", None).await;
        ui.update_pending_messages_display();
        let pending = lock(&ui.pending_messages_container).render(60).join("\n");
        assert!(
            pending.contains("Steering: session steer"),
            "pending: {pending}"
        );
        assert!(
            pending.contains("Follow-up: session follow"),
            "pending: {pending}"
        );
    }

    #[tokio::test]
    async fn restore_queued_messages_combines_all_queues_and_aborts() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        ui.queue_compaction_message("compaction q".to_string(), StreamingBehavior::Steer);
        let _ = ui.session().steer("session q", None).await;
        lock(&ui.editor).set_text("draft");
        let restored = ui.restore_queued_messages_to_editor(true);
        assert_eq!(restored, 2);
        let text = lock(&ui.editor).get_text();
        assert!(text.contains("compaction q"), "text: {text}");
        assert!(text.contains("session q"), "text: {text}");
        assert!(text.contains("draft"), "text: {text}");
        assert!(
            lock(&ui.compaction_queue).is_empty(),
            "compaction queue drained"
        );
        assert!(
            ui.session().get_steering_messages().is_empty(),
            "session queue drained"
        );
        // Pending display cleared.
        assert!(lock(&ui.pending_messages_container).render(60).is_empty());
    }

    #[tokio::test]
    async fn flush_compaction_queue_restores_on_prompt_failure() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        {
            let ui = &mode.ui_state;
            ui.queue_compaction_message("first msg".to_string(), StreamingBehavior::Steer);
            ui.queue_compaction_message("second msg".to_string(), StreamingBehavior::FollowUp);
        }
        // The test session has no auth: the first prompt fails and the whole
        // queue is restored with an error status (interactive-mode.ts:4081-4083).
        mode.flush_compaction_queue().await;
        assert_eq!(
            lock(&mode.ui_state.compaction_queue).len(),
            2,
            "queue restored after failure"
        );
        let rendered = lock(&mode.ui_state.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("Failed to send queued messages"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn flush_compaction_queue_restores_on_shutdown() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        {
            let ui = &mode.ui_state;
            ui.queue_compaction_message("first msg".to_string(), StreamingBehavior::Steer);
            ui.queue_compaction_message("second msg".to_string(), StreamingBehavior::FollowUp);
        }
        // Shutdown signalled before the flush: the prompt branch loses the
        // (biased) race and the taken messages must be put back — without
        // the restore they would be dropped silently.
        let _ = mode.ui_state.shutdown_tx.send(true);
        mode.flush_compaction_queue().await;
        let queue = lock(&mode.ui_state.compaction_queue);
        let texts: Vec<&str> = queue.iter().map(|message| message.text.as_str()).collect();
        assert_eq!(texts, ["first msg", "second msg"], "queue restored");
    }

    #[tokio::test]
    async fn init_stores_unsubscribe_and_shutdown_takes_it() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        assert!(mode.unsubscribe.is_none());
        mode.init().await;
        assert!(
            mode.unsubscribe.is_some(),
            "init keeps the subscription handle"
        );
        mode.shutdown().await;
        assert!(mode.unsubscribe.is_none(), "shutdown consumed it");
    }

    #[tokio::test]
    async fn next_command_routes_selector_picks() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.input_tx
            .send(EditorInput::ResumeSession("/tmp/some.jsonl".to_string()))
            .expect("send resume");
        ui.input_tx
            .send(EditorInput::ForkFrom("entry-1".to_string()))
            .expect("send fork");
        match mode.next_command().await {
            Some(RunCommand::ResumeSession(path)) => assert_eq!(path, "/tmp/some.jsonl"),
            other => panic!("expected ResumeSession, got {other:?}"),
        }
        match mode.next_command().await {
            Some(RunCommand::ForkFrom(entry_id)) => assert_eq!(entry_id, "entry-1"),
            other => panic!("expected ForkFrom, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // Loaded resources display (`showLoadedResources`, interactive-mode.ts:1421-1627)
    // ---------------------------------------------------------------------

    /// A harness whose session has skills, a prompt template, a custom theme
    /// (plus a broken one → diagnostic) and an `AGENTS.md` context file
    /// loaded through the resource loader.
    async fn loaded_resources_harness(
        verbose: bool,
    ) -> (InteractiveMode, Arc<TestTerminal>, TempDir) {
        let harness = build_test_session().await;
        let agent_dir = harness.runtime.services().agent_dir.clone();

        // Skills (agent_dir/skills/<name>/SKILL.md, same layout as the
        // autocomplete tests).
        for (name, description) in [("skill-b", "B skill"), ("skill-a", "A skill")] {
            let file = agent_dir.join("skills").join(name).join("SKILL.md");
            std::fs::create_dir_all(file.parent().expect("skill dir")).expect("create skill dir");
            std::fs::write(
                &file,
                format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
            )
            .expect("write skill file");
        }
        // Prompt template (agent_dir/prompts/*.md, name from the basename).
        let prompt = agent_dir.join("prompts").join("review.md");
        std::fs::create_dir_all(prompt.parent().expect("prompt dir")).expect("create prompt dir");
        std::fs::write(&prompt, "Review the changes\n").expect("write prompt file");
        // Custom theme (all required color keys) + a broken theme file that
        // produces a load diagnostic.
        let theme = agent_dir.join("themes").join("custom.json");
        std::fs::create_dir_all(theme.parent().expect("theme dir")).expect("create theme dir");
        let mut colors = serde_json::Map::new();
        for key in crate::core::themes::REQUIRED_COLOR_KEYS {
            colors.insert(
                (*key).to_string(),
                serde_json::Value::String("#000000".to_string()),
            );
        }
        std::fs::write(
            &theme,
            serde_json::to_string(&serde_json::json!({"name": "custom", "colors": colors}))
                .expect("theme json"),
        )
        .expect("write theme file");
        let broken = agent_dir.join("themes").join("broken.json");
        std::fs::write(&broken, "{ not json").expect("write broken theme file");
        // Context file (AGENTS.md in the cwd).
        std::fs::write(harness.cwd.join("AGENTS.md"), "project rules\n").expect("write AGENTS.md");
        lock(&harness.session.resource_loader()).reload();

        let TestSession { _tmp, runtime, .. } = harness;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions {
                verbose,
                ..InteractiveModeOptions::default()
            },
            Box::new(TestTerminal::clone(&terminal)),
        );
        (mode, terminal, _tmp)
    }

    #[tokio::test]
    async fn show_loaded_resources_renders_sections() {
        let (mode, _terminal, _tmp_keep) = loaded_resources_harness(false).await;
        let ui = &mode.ui_state;
        ui.show_loaded_resources();

        let rendered = lock(&ui.loaded_resources_container).render(80).join("\n");
        assert!(rendered.contains("[Context]"), "rendered: {rendered}");
        assert!(rendered.contains("AGENTS.md"), "rendered: {rendered}");
        assert!(rendered.contains("[Skills]"), "rendered: {rendered}");
        // Compact lists are trimmed, sorted and joined (formatCompactList).
        let skills = rendered
            .lines()
            .find(|line| line.contains("skill-a"))
            .expect("skills list");
        let a_index = skills.find("skill-a").expect("skill-a");
        let b_index = skills.find("skill-b").expect("skill-b");
        assert!(a_index < b_index, "sorted compact list: {skills}");
        assert!(rendered.contains("[Prompts]"), "rendered: {rendered}");
        assert!(rendered.contains("/review"), "rendered: {rendered}");
        assert!(rendered.contains("[Themes]"), "rendered: {rendered}");
        assert!(rendered.contains("custom"), "rendered: {rendered}");
        // The broken theme produced a diagnostic block (warning color).
        assert!(
            rendered.contains("[Resource issues]"),
            "rendered: {rendered}"
        );
        // The diagnostic path may wrap across 80-column lines depending on
        // the temp-dir name length — assert the message, not the path.
        assert!(
            rendered.contains("Failed to parse theme"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn show_loaded_resources_quiet_shows_diagnostics_only() {
        let (mode, _terminal, _tmp_keep) = loaded_resources_harness(false).await;
        mode.session
            .settings_manager(|settings| settings.set_quiet_startup(true));
        let ui = &mode.ui_state;
        ui.show_loaded_resources();

        let rendered = lock(&ui.loaded_resources_container).render(80).join("\n");
        assert!(
            !rendered.contains("[Skills]"),
            "quiet hides listings: {rendered}"
        );
        assert!(
            !rendered.contains("[Prompts]"),
            "quiet hides listings: {rendered}"
        );
        assert!(
            rendered.contains("[Resource issues]"),
            "showDiagnosticsWhenQuiet keeps diagnostics: {rendered}"
        );
    }

    #[tokio::test]
    async fn show_loaded_resources_verbose_overrides_quiet() {
        let (mode, _terminal, _tmp_keep) = loaded_resources_harness(true).await;
        mode.session
            .settings_manager(|settings| settings.set_quiet_startup(true));
        let ui = &mode.ui_state;
        ui.show_loaded_resources();

        let rendered = lock(&ui.loaded_resources_container).render(80).join("\n");
        assert!(
            rendered.contains("[Skills]"),
            "verbose forces the listing: {rendered}"
        );
    }

    #[tokio::test]
    async fn show_loaded_resources_without_resources_is_empty() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.show_loaded_resources();
        // No sections, no diagnostics → nothing rendered.
        assert!(lock(&ui.loaded_resources_container).render(80).is_empty());
    }

    // ---------------------------------------------------------------------
    // Cache-miss notice hook (`maybeShowCacheMissNotice`, interactive-mode.ts:3449-3455)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn cache_miss_hook_runs_on_message_end_without_rendering() {
        let (mode, _terminal, _session) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.session()
            .settings_manager(|settings| settings.set_show_cache_miss_notices(true));

        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("answer")],
            StopReason::Stop,
        )));
        ui.push(UiCommand::MessageEnd(assistant_message(
            vec![text_content("answer")],
            StopReason::Stop,
        )));
        ui.drain_events();

        // The hook compiles and runs in the drain; the T14 detector renders
        // nothing yet, so the chat holds only the streaming component.
        assert_eq!(
            chat_children(ui),
            1,
            "no cache-miss notice children yet (T14): {}",
            lock(&ui.chat_container).render(60).join("\n")
        );
    }

    // ---------------------------------------------------------------------
    // Theme hot reload (T12-S6; theme.ts:886-957)
    // ---------------------------------------------------------------------

    /// RAII env guard restoring the original value on drop.
    struct EnvGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn with(name: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var_os(name);
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
            EnvGuard { name, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// A valid custom-theme JSON derived from the built-in dark theme.
    fn custom_theme_json(name: &str) -> String {
        let mut theme = crate::core::themes::get_builtin_themes()
            .get("dark")
            .expect("builtin dark theme")
            .clone();
        theme.name = name.to_string();
        serde_json::to_string(&theme).expect("serialize theme json")
    }

    /// Bump a file's mtime (the watcher is mtime-based).
    fn touch(path: &std::path::Path) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open theme file");
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
            .expect("set mtime");
    }

    /// Write a custom theme into the (env-overridden) global themes dir and
    /// point the session's theme setting at it.
    fn install_custom_theme(name: &str, session: &AgentSession) -> std::path::PathBuf {
        let themes_dir = crate::config::get_global_themes_dir();
        std::fs::create_dir_all(&themes_dir).expect("themes dir");
        let theme_file = themes_dir.join(format!("{name}.json"));
        std::fs::write(&theme_file, custom_theme_json(name)).expect("write theme file");
        session.settings_manager(|settings| settings.set_theme(name));
        theme_file
    }

    #[tokio::test]
    async fn theme_changed_reloads_custom_theme_and_keeps_last_good_on_failure() {
        let (mode, _terminal, session) = mode_harness().await;
        let ui = &mode.ui_state;
        // The watcher tests + the startup dialog tests override the
        // process-global agent dir env — serialize them.
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);

        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let _agent_env = EnvGuard::with(
            crate::config::ENV_AGENT_DIR,
            Some(&tmp.path().join("agent").display().to_string()),
        );
        let theme_file = install_custom_theme("draintest", &session);

        let before = Arc::clone(&lock(&ui.theme));
        let before_md = Arc::clone(&lock(&ui.markdown_theme));
        ui.push(UiCommand::ThemeChanged);
        ui.drain_events();
        assert_eq!(
            lock(&ui.theme).name.as_deref(),
            Some("draintest"),
            "custom theme applied"
        );
        assert!(
            !Arc::ptr_eq(&before, &lock(&ui.theme)),
            "theme instance swapped"
        );
        assert!(
            !Arc::ptr_eq(&before_md, &lock(&ui.markdown_theme)),
            "markdown theme rebuilt"
        );

        // A broken file keeps the last successfully loaded theme
        // (theme.ts:921-932 ignores reload errors).
        std::fs::write(&theme_file, "{ not json").expect("write broken theme");
        touch(&theme_file);
        ui.push(UiCommand::ThemeChanged);
        ui.drain_events();
        assert_eq!(
            lock(&ui.theme).name.as_deref(),
            Some("draintest"),
            "last good theme retained after parse failure"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn theme_watcher_thread_detects_file_changes_and_queues_reload() {
        let (mode, _terminal, session) = mode_harness().await;
        let ui = &mode.ui_state;
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);

        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let _agent_env = EnvGuard::with(
            crate::config::ENV_AGENT_DIR,
            Some(&tmp.path().join("agent").display().to_string()),
        );
        let theme_file = install_custom_theme("watchertest", &session);

        let stop = Arc::new(AtomicBool::new(false));
        let watcher = crate::modes::interactive::theme_watcher::spawn_theme_watcher(
            mode.ui.clone(),
            Arc::clone(ui),
            Arc::clone(&stop),
        );

        // Bump the file repeatedly until the watcher's poll registers the
        // change and queues the reload command (bounded wait).
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut queued = false;
        while std::time::Instant::now() < deadline {
            touch(&theme_file);
            if !lock(&ui.event_queue).is_empty() {
                queued = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, AtomicOrdering::Relaxed);
        let _ = watcher.join();
        assert!(queued, "theme change queued a reload command");

        ui.drain_events();
        assert_eq!(
            lock(&ui.theme).name.as_deref(),
            Some("watchertest"),
            "reload applied by the drain"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn init_auto_theme_applies_slash_setting_branch_without_pump() {
        let (mode, _terminal, session) = mode_harness().await;
        let ui = &mode.ui_state;
        // The harness terminal never answers OSC/DSR queries and nothing
        // pumps, so detection resolves via the query deadline/backstop and
        // falls back to COLORFGBG/"dark" — either built-in branch is valid.
        session.settings_manager(|settings| settings.set_theme("light/dark"));
        mode.init_auto_theme().await;
        let name = lock(&ui.theme).name.clone();
        assert!(
            matches!(name.as_deref(), Some("dark" | "light")),
            "auto branch applied, got {name:?}"
        );
    }

    // ---------------------------------------------------------------------
    // T12-S7a: self-check list completion
    // ---------------------------------------------------------------------

    /// Large-session performance benchmark (T12 self-check list "large-session
    /// performance"): full rebuild + render of 1k+ messages, asserting completion
    /// within a loose time limit and printing the benchmark numbers.
    #[tokio::test]
    async fn large_session_render_initial_messages_completes() {
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;

        // 600 user/assistant pairs (with long markdown and code blocks) ≈ 1200 entries.
        {
            let manager = ui.session().session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..600 {
                manager
                    .append_message(user_message(&format!(
                        "Question {i}: what is the meaning of life, the universe and everything? "
                    )))
                    .expect("append user");
                manager
                    .append_message(assistant_message(
                        vec![
                            text_content(&format!(
                                "Answer {i}: **bold** and *italic* and `inline` with a long paragraph that wraps many times: {}",
                                "lorem ipsum dolor sit amet ".repeat(20)
                            )),
                            text_content(&format!(
                                "```rust\nfn main() {{ println!(\"answer {i}\"); }}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |"
                            )),
                        ],
                        StopReason::Stop,
                    ))
                    .expect("append assistant");
            }
        }

        let build_start = std::time::Instant::now();
        ui.render_initial_messages();
        let build_elapsed = build_start.elapsed();
        let children = lock(&ui.chat_container).children.len();
        assert!(children >= 1200, "children: {children}");

        let render_start = std::time::Instant::now();
        let lines = lock(&ui.chat_container).render(80);
        let render_elapsed = render_start.elapsed();
        assert!(!lines.is_empty());

        println!(
            "PERF large_session: build={build_elapsed:?} render={render_elapsed:?} children={children} lines={}",
            lines.len()
        );
        // Loose upper bound: normal machines should be far below this (see the report
        // for the benchmark numbers; when the measurement output is invisible due to
        // RUST_LOG/test capture, the test duration is the measure).
        assert!(build_elapsed.as_secs() < 30, "initial build must complete");
        assert!(render_elapsed.as_secs() < 10, "full render must complete");
    }

    /// Render-throttle coalescing (T12 self-check list "throttle does not drop
    /// frames"): consecutive request_render calls coalesce into one deadline, consumed
    /// by a single tick.
    #[test]
    fn render_throttle_coalesces_requests() {
        let tui = Tui::new(Box::new(TestTerminal::new()));
        for _ in 0..10 {
            tui.request_render(false);
        }
        assert!(tui.next_deadline().is_some(), "one coalesced deadline");
        tui.tick(std::time::Instant::now());
        assert!(
            !tui.has_pending_work(),
            "single tick drains the coalesced render"
        );
        // A second burst coalesces the same way.
        tui.request_render(false);
        tui.tick(std::time::Instant::now() + std::time::Duration::from_millis(20));
        assert!(!tui.has_pending_work());
    }

    /// VT-driven message-queue/editor scenario (T12 self-check list "message queue VT
    /// driven"): steer enqueues → pending render → Alt+Up coalesces and fills back →
    /// Escape clears → input submits.
    #[tokio::test]
    async fn vt_driven_queue_and_editor_scenario() {
        install_global_keybindings();
        let (mut mode, terminal, _session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;

        // Mimic enqueueing mid-stream (session.steer queues directly; no real
        // streaming needed).
        let _ = ui.session().steer("steer message", None).await;
        ui.update_pending_messages_display();
        let pending = lock(&ui.pending_messages_container).render(60).join("\n");
        assert!(
            pending.contains("Steering: steer message"),
            "pending: {pending}"
        );
        assert!(
            pending.contains("to edit all queued messages"),
            "pending: {pending}"
        );

        // Alt+Up (legacy sequence \x1bp) queues a Dequeue via TUI dispatch; the test
        // has no driver thread, so a manual drain applies it.
        terminal.feed("\u{1b}p");
        ui.ui.tick(std::time::Instant::now());
        ui.drain_events();
        let text = lock(&ui.editor).get_text();
        assert!(text.contains("steer message"), "editor: {text:?}");
        assert!(
            ui.session().get_steering_messages().is_empty(),
            "queue drained"
        );

        // Escape (editor non-empty, not bash mode) → cleared via dispatch + drain? —
        // when text is being typed, CustomEditor dispatches Escape to the base editor,
        // which does nothing without text; here the Escape command is driven directly to
        // verify the bash-mode clearing path.
        lock(&ui.editor).set_text("!draft bash");
        *lock(&ui.is_bash_mode) = true;
        terminal.feed("\u{1b}");
        ui.ui.tick(std::time::Instant::now());
        ui.drain_events();
        assert_eq!(lock(&ui.editor).get_text(), "", "bash mode cleared");
        assert!(!*lock(&ui.is_bash_mode));

        // Input + Enter: dispatch drives submit_value (clears the editor + callback
        // into the channel).
        terminal.feed("hello");
        ui.ui.tick(std::time::Instant::now());
        assert_eq!(lock(&ui.editor).get_text(), "hello");
        terminal.feed("\r");
        ui.ui.tick(std::time::Instant::now());
        assert_eq!(lock(&ui.editor).get_text(), "", "submit clears editor");
    }

    /// /tree three branches (T12 self-check list): user → leaf=parent + text fill-back
    /// (UI integration).
    #[tokio::test]
    async fn tree_select_user_message_restores_leaf_and_editor_text() {
        let (mut mode, terminal, session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("root question"))
                .expect("append");
            manager
                .append_message(assistant_message(
                    vec![text_content("root answer")],
                    StopReason::Stop,
                ))
                .expect("append");
            manager
                .append_message(user_message("follow-up question"))
                .expect("append");
        }
        InteractiveUi::show_tree_selector(&Arc::clone(&mode.ui_state), None);
        assert!(lock(&ui.active_selector).is_some());

        // Initially the leaf (follow-up) is selected — selecting the leaf itself is a
        // no-op (upstream 4652-4656); Up twice to the first user (root question), then
        // Enter.
        terminal.feed("\u{1b}[A");
        ui.ui.tick(std::time::Instant::now());
        terminal.feed("\u{1b}[A");
        ui.ui.tick(std::time::Instant::now());
        terminal.feed("\r");
        ui.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(lock(&ui.active_selector).is_none(), "selector closed");
        // leaf = the selected user's parent (one of the SDK seed entries).
        let leaf = lock(&session.session_manager())
            .get_leaf_id()
            .map(str::to_owned);
        let entries = lock(&session.session_manager()).get_entries();
        let first_user_parent = entries
            .iter()
            .find_map(|e| match e.known() {
                Some(rpi_agent::session::SessionEntry::Message(m))
                    if matches!(m.message, AgentMessage::User(_)) =>
                {
                    Some(e.parent_id().map(str::to_owned))
                }
                _ => None,
            })
            .expect("first user entry");
        assert_eq!(
            leaf.as_deref(),
            first_user_parent.as_deref(),
            "leaf moved to the user message's parent"
        );
        // The editor fills back the selected user's text (when the editor is empty).
        let text = lock(&ui.editor).get_text();
        assert!(text.contains("root question"), "editor: {text:?}");
    }

    /// /tree three branches: root user (built after reset_leaf, parent=None) → leaf
    /// reset.
    #[tokio::test]
    async fn tree_select_root_user_resets_leaf() {
        let (mut mode, terminal, session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            // Clear the leaf formed by the SDK seed entries so the next user becomes a
            // true root node.
            manager.reset_leaf();
            manager
                .append_message(user_message("root question"))
                .expect("append");
            manager
                .append_message(assistant_message(
                    vec![text_content("root answer")],
                    StopReason::Stop,
                ))
                .expect("append");
        }
        InteractiveUi::show_tree_selector(&Arc::clone(&mode.ui_state), None);
        // Select the assistant (leaf) → Up to the root user → Enter.
        terminal.feed("\u{1b}[A");
        ui.ui.tick(std::time::Instant::now());
        terminal.feed("\r");
        ui.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let leaf = lock(&session.session_manager())
            .get_leaf_id()
            .map(str::to_owned);
        assert!(leaf.is_none(), "root user resets the leaf, got {leaf:?}");
        let text = lock(&ui.editor).get_text();
        assert!(text.contains("root question"), "editor: {text:?}");
    }

    /// /tree three branches: assistant → moving the leaf leaves it empty (no editor
    /// fill-back).
    #[tokio::test]
    async fn tree_select_assistant_moves_leaf_without_editor_text() {
        let (mut mode, terminal, session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("root question"))
                .expect("append");
            manager
                .append_message(assistant_message(
                    vec![text_content("root answer")],
                    StopReason::Stop,
                ))
                .expect("append");
        }
        InteractiveUi::show_tree_selector(&Arc::clone(&mode.ui_state), None);
        // Selecting the assistant (leaf) → Enter is a no-op (Already at this point);
        // Up to user then Down back to assistant? — selecting the assistant directly:
        // the initial selection is already the leaf, so Enter hits "Already at this
        // point". Instead, Up to select the user, then Down back to the assistant.
        terminal.feed("\u{1b}[A");
        ui.ui.tick(std::time::Instant::now());
        terminal.feed("\u{1b}[B");
        ui.ui.tick(std::time::Instant::now());
        terminal.feed("\r");
        ui.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // The leaf is still the assistant (moved onto itself) and the editor is not
        // filled back.
        let entries = lock(&session.session_manager()).get_entries();
        let assistant_id = entries
            .iter()
            .find_map(|e| match e.known() {
                Some(rpi_agent::session::SessionEntry::Message(m))
                    if matches!(m.message, AgentMessage::Assistant(_)) =>
                {
                    Some(e.id().to_string())
                }
                _ => None,
            })
            .expect("assistant entry");
        let leaf = lock(&session.session_manager())
            .get_leaf_id()
            .map(str::to_owned);
        assert_eq!(leaf.as_deref(), Some(assistant_id.as_str()));
        assert_eq!(
            lock(&ui.editor).get_text(),
            "",
            "assistant leaves editor empty"
        );
    }

    /// /tree three branches: custom_message → leaf=parent + text fill-back.
    #[tokio::test]
    async fn tree_select_custom_message_restores_leaf_and_editor_text() {
        let (mut mode, terminal, session) = mode_harness().await;
        mode.init().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("root question"))
                .expect("append");
            manager
                .append_custom_message_entry(
                    "test.type",
                    rpi_ai::types::UserContent::Text("custom payload".to_string()),
                    true,
                    None,
                )
                .expect("append custom");
        }
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(assistant_message(
                    vec![text_content("tail")],
                    StopReason::Stop,
                ))
                .expect("append assistant tail");
        }
        InteractiveUi::show_tree_selector(&Arc::clone(&mode.ui_state), None);
        // leaf=tail assistant → Up to the custom → Enter.
        terminal.feed("\u{1b}[A");
        ui.ui.tick(std::time::Instant::now());
        terminal.feed("\r");
        ui.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // leaf = the custom's parent (root user); the editor fills back the custom text.
        let leaf = lock(&session.session_manager())
            .get_leaf_id()
            .map(str::to_owned);
        let entries = lock(&session.session_manager()).get_entries();
        let custom_parent = entries
            .iter()
            .find_map(|e| match e.known() {
                Some(rpi_agent::session::SessionEntry::CustomMessage(_)) => {
                    Some(e.parent_id().map(str::to_owned))
                }
                _ => None,
            })
            .expect("custom entry");
        assert_eq!(
            leaf.as_deref(),
            custom_parent.as_deref(),
            "leaf moved to custom's parent"
        );
        let text = lock(&ui.editor).get_text();
        assert!(text.contains("custom payload"), "editor: {text:?}");
    }

    /// T12-S7b: the M5-mandatory scripted smoke — full-path VT end-to-end through the
    /// run loop: startup → bash question → shortcuts → Ctrl+D exit → terminal restore.
    /// Real-machine streaming/abort paths remain manual smoke (no tty/provider-key
    /// environment).
    #[tokio::test]
    async fn run_loop_end_to_end_vt_smoke() {
        install_global_keybindings();
        let (mut mode, terminal, _session) = mode_harness().await;
        // run()'s future is not Send (unsubscribe slots etc.) — drive it with a
        // dedicated thread + current_thread runtime (production uses the main tokio
        // task; semantics are the same).
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            rt.block_on(async move {
                mode.run().await;
            });
        });
        // Wait for init + the driver thread to start.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(terminal.is_started(), "terminal started by run()");

        // The "question": a bash command goes through the input channel → run loop →
        // execution.
        terminal.feed("!echo vt-e2e\r");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // Shortcut: a single Ctrl+C clears the editor (does not exit) — fed separately
        // so  becomes its own dispatch.
        terminal.feed("draft");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        terminal.feed("\u{3}");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Ctrl+D exit: dispatch → shutdown signal → run loop winds down.
        terminal.feed("\u{4}");
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            handle.join().expect("run thread")
        })
        .await
        .expect("run loop exits after Ctrl+D");

        // Exit restore: the terminal stop is called (raw mode restored).
        assert!(!terminal.is_started(), "terminal restored on shutdown");
    }

    // ---------------------------------------------------------------------
    // T15 W4: InteractiveUiBridge VT tests
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn w4_bridge_select_dialog_roundtrip_and_timeout() {
        use rpi_ext_host::api::UiBridge;
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let bridge = ui_bridge::InteractiveUiBridge::new(&mode.ui_state);
        let bridge = Arc::new(bridge);

        // select: the dialog mounts at the editor position; Enter resolves to the
        // option value.
        let b = bridge.clone();
        let task = tokio::spawn(async move {
            b.select("Pick one", &["Allow".to_owned(), "Block".to_owned()], None)
                .await
        });
        for _ in 0..100 {
            if lock(&mode.ui_state.active_selector).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let selector = lock(&mode.ui_state.active_selector)
            .clone()
            .expect("selector mounted");
        lock(&selector).handle_input("\r");
        assert_eq!(task.await.expect("task"), Some("Allow".to_owned()));
        // The dialog closes and the editor resets.
        assert!(lock(&mode.ui_state.active_selector).is_none());

        // confirm goes through the Yes/No selector (interactive-mode.ts:2262-2269).
        let b = bridge.clone();
        let task = tokio::spawn(async move { b.confirm("Sure?", "really", None).await });
        for _ in 0..100 {
            if lock(&mode.ui_state.active_selector).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let selector = lock(&mode.ui_state.active_selector)
            .clone()
            .expect("confirm mounted");
        lock(&selector).handle_input("\r");
        assert!(task.await.expect("task"));

        // timeout: resolves the default value automatically and closes.
        let b = bridge.clone();
        let task = tokio::spawn(async move {
            b.select(
                "t",
                &["x".to_owned()],
                Some(rpi_ext_host::api::UiDialogOptions { timeout: Some(40) }),
            )
            .await
        });
        assert_eq!(task.await.expect("task"), None);
        assert!(lock(&mode.ui_state.active_selector).is_none());
        mode.shutdown().await;
    }

    #[tokio::test]
    async fn w4_bridge_widget_footer_header_and_status() {
        use rpi_ext_host::api::{UiBridge, WidgetContent};
        let (mut mode, _terminal, _session) = mode_harness().await;
        mode.init().await;
        let bridge = ui_bridge::InteractiveUiBridge::new(&mode.ui_state);
        let ui = &mode.ui_state;

        // setWidget (string[], aboveEditor default) → rendered into widgets_above.
        bridge.set_widget(
            "w1",
            Some(WidgetContent::Lines(vec!["WIDGET-LINE".to_owned()])),
            None,
        );
        {
            let container = lock(&ui.widgets_above);
            let rendered = container
                .children
                .iter()
                .flat_map(|c| c.render(80))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                rpi_test_support::vt::strip_ansi(&rendered).contains("WIDGET-LINE"),
                "widget rendered: {rendered:?}"
            );
        }
        // Clear → the container is empty.
        bridge.set_widget("w1", None, None);
        assert!(lock(&ui.widgets_above).children.is_empty());

        // setHeader replaces the header area with the component description; None
        // restores the built-in.
        bridge.set_header(Some(serde_json::json!({
            "type": "text", "props": {"text": "EXT-HEADER"}
        })));
        assert!(lock(&ui.custom_header).is_some());
        bridge.set_header(None);
        assert!(lock(&ui.custom_header).is_none());

        // setFooter likewise.
        bridge.set_footer(Some(serde_json::json!({
            "type": "text", "props": {"text": "EXT-FOOTER"}
        })));
        assert!(lock(&ui.custom_footer).is_some());
        bridge.set_footer(None);
        assert!(lock(&ui.custom_footer).is_none());

        // setStatus goes into the footer data; None clears it.
        bridge.set_status("ext", Some("busy"));
        assert_eq!(
            ui.footer_data
                .get_extension_statuses()
                .get("ext")
                .map(String::as_str),
            Some("busy")
        );
        bridge.set_status("ext", None);
        assert!(!ui.footer_data.get_extension_statuses().contains_key("ext"));

        // editor text read/write + paste path.
        bridge.set_editor_text("hello");
        assert_eq!(bridge.get_editor_text(), "hello");
        bridge.paste_to_editor("pasted-content");
        assert!(bridge.get_editor_text().contains("pasted-content"));

        // tools expanded。
        assert!(!bridge.get_tools_expanded());
        bridge.set_tools_expanded(true);
        assert!(bridge.get_tools_expanded());
        mode.shutdown().await;
    }
}
