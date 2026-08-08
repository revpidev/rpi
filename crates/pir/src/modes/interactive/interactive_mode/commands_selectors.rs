//! Slash-command handlers and selector wiring for the interactive mode
//! (T12-S5b, group B: selector commands + bash mode).
//!
//! Upstream: `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
//! @ pi 0.82.1 (2efa728) — selectors at interactive-mode.ts:4135-5081,
//! `handleReloadCommand` 5318-5419, `handleClearCommand` (the `/new` path)
//! 5859-5876, `handleBashCommand` 5931-6016.
//!
//! This module is a *submodule* of `interactive_mode`, so it can extend the
//! private `InteractiveUi` / `InteractiveMode` impls without widening any
//! visibility (Rust private items are visible to descendant modules).
//!
//! Intentional differences vs upstream:
//! - Settings `onThemeChange` hot-applies through the drain
//!   (`UiCommand::ApplyThemeName`) instead of an async theme controller: the
//!   drain owns [`InteractiveUi::apply_theme`] (lock contract), and automatic
//!   pairs resolve the terminal appearance off the driver thread first.
//!   `ThemePreview` is ignored (the component documents this).
//! - Selector callbacks run on the TUI driver thread without runtime access;
//!   session picks (session selector, user-message fork selector) are routed
//!   to the run loop over the `EditorInput` channel and executed by
//!   `InteractiveMode` (`handle_resume_command` / `handle_fork_command`).
//! - Login/logout selectors and the api-key login flow are wired (T14 W6b);
//!   the OAuth login dialog flow stays a stub (T13 leftover).
//! - `handleBashCommand`: local `AgentSession::execute_bash` already records
//!   the result internally (agent-session.rs:2076), so this port does not
//!   call `record_bash_result` again (upstream interactive-mode.ts:5967).
//! - Session switches (`/new`, `/resume`, `/clone`, `/fork`, `/import`)
//!   rebind through `InteractiveMode::rebind_session_ui` (upstream
//!   `rebindCurrentSession`, interactive-mode.ts:1732-1758) instead of the
//!   runtime's `setRebindSession` hook, which stays `None`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pir_ai::auth::interaction::{AuthEvent, AuthInteraction, AuthPrompt};
use pir_ai::auth::types::{AuthCheck, AuthType, BoxFutureSend, CredentialType};
use pir_ai::auth::{ModelsError, ModelsErrorCode};
use pir_ai::types::{Model, ModelThinkingLevel, ThinkingLevel};
use pir_tui::terminal_colors::TerminalColorScheme;
use pir_tui::tui::{shared_component_from_boxed, Component, Focusable};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    child_address, install_global_keybindings, lock, remove_child_by_address, EditorInput,
    FocusableRegion, InteractiveMode, InteractiveUi, SharedChild, UiCommand,
};
use crate::core::agent_session::{
    AgentSession, BashChunkCallback, CycleDirection, ExecuteBashOptions, ExtensionBindings,
};
use crate::core::agent_session_runtime::ForkPosition;
use crate::core::model_resolver::{
    default_model_for_provider, find_exact_model_reference_match,
    resolve_model_scope_with_diagnostics,
};
use crate::core::model_runtime::ModelRuntime;
use crate::core::session_manager::SessionManager;
use crate::core::themes::get_available_themes;
use crate::core::trust_manager::ProjectTrustStore;
use crate::extensions::llama::{LlamaHost, NotifyLevel};
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::extension_selector::ExtensionSelectorComponent;
use crate::modes::interactive::components::llama_view::new_llama_view;
use crate::modes::interactive::components::login_dialog::LoginDialogComponent;
use crate::modes::interactive::components::model_selector::ModelSelectorComponent;
use crate::modes::interactive::components::oauth_selector::{
    AuthSelectorMode, AuthSelectorProvider, OAuthSelectorComponent,
};
use crate::modes::interactive::components::scoped_models_selector::ScopedModelsSelectorComponent;
use crate::modes::interactive::components::session_selector::SessionSelectorComponent;
use crate::modes::interactive::components::settings_selector::{
    SettingsChange, SettingsSelectorComponent, SettingsSelectorOptions,
};
use crate::modes::interactive::components::trust_selector::{TrustOption, TrustSelectorComponent};
use crate::modes::interactive::theme_watcher::{auto_theme_pair, detect_terminal_theme_for_auto};
use crate::tools::truncate::TruncationResult;

// =============================================================================
// Helpers
// =============================================================================

/// Focus wrapper for selectors that do not implement [`Focusable`]
/// themselves (e.g. `TrustSelectorComponent`): `FocusableRegion` requires
/// `T: Focusable`, so the mount point keeps its own focus flag.
struct FocusableShell<T: Component> {
    inner: Arc<Mutex<T>>,
    focused: bool,
}

impl<T: Component> Component for FocusableShell<T> {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.inner).render(width)
    }

    fn handle_input(&mut self, data: &str) {
        lock(&self.inner).handle_input(data);
    }

    fn invalidate(&mut self) {
        lock(&self.inner).invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl<T: Component> Focusable for FocusableShell<T> {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

/// `pir_agent::types::ThinkingLevel` is a re-export of
/// `pir_ai::types::ModelThinkingLevel` (types.rs:30-32), while the settings
/// selector uses the plain `pir_ai::types::ThinkingLevel` (no `Off`). `Off`
/// maps to `Minimal` — the settings list has no "off" entry.
fn model_thinking_to_setting(level: ModelThinkingLevel) -> ThinkingLevel {
    match level {
        ModelThinkingLevel::Off | ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
        ModelThinkingLevel::Low => ThinkingLevel::Low,
        ModelThinkingLevel::Medium => ThinkingLevel::Medium,
        ModelThinkingLevel::High => ThinkingLevel::High,
        ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
        ModelThinkingLevel::Max => ThinkingLevel::Max,
    }
}

fn setting_to_model_thinking(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

/// `formatHttpIdleTimeoutMs` (http-dispatcher.ts:27-38).
fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    const CHOICES: &[(u64, &str)] = &[
        (30_000, "30 sec"),
        (60_000, "1 min"),
        (120_000, "2 min"),
        (300_000, "5 min"),
        (0, "disabled"),
    ];
    CHOICES
        .iter()
        .find(|(timeout, _)| *timeout == timeout_ms)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| format!("{} sec", timeout_ms / 1000))
}

/// `getProjectTrustOptions` (trust-manager.ts:52-74): the five options
/// (session-only variants included) as local [`TrustOption`]s. The `value`
/// strings are interpreted by [`InteractiveUi::show_trust_selector`].
fn build_trust_options(cwd: &Path) -> Vec<TrustOption> {
    let mut options = vec![TrustOption {
        value: "trust".to_string(),
        label: "Trust".to_string(),
        description: Some(format!("Trust {}", cwd.display())),
    }];
    if let Some(parent) = cwd.parent() {
        options.push(TrustOption {
            value: "trust-parent".to_string(),
            label: format!("Trust parent folder ({})", parent.display()),
            description: Some(format!(
                "Trust {} and clear the decision for {}",
                parent.display(),
                cwd.display()
            )),
        });
    }
    options.push(TrustOption {
        value: "trust-session".to_string(),
        label: "Trust (this session only)".to_string(),
        description: None,
    });
    options.push(TrustOption {
        value: "untrust".to_string(),
        label: "Do not trust".to_string(),
        description: Some(format!("Do not trust {}", cwd.display())),
    });
    options.push(TrustOption {
        value: "untrust-session".to_string(),
        label: "Do not trust (this session only)".to_string(),
        description: None,
    });
    options
}

// =============================================================================
// Selector commands
// =============================================================================

/// `applyFromSettings` auto branch (theme-controller.ts:40-44): enable
/// color-scheme follow (`setAutoSync`), detect the terminal appearance, then
/// apply the matching pair branch through the drain. The detection awaits
/// terminal replies pumped by the driver thread, so it must run off the
/// driver thread: on the ambient runtime when there is one, otherwise on a
/// dedicated thread with a tiny runtime (same pattern as
/// `register_signal_handlers`).
fn apply_auto_theme_pair(ui: &Arc<InteractiveUi>, light: String, dark: String) {
    ui.ui.set_terminal_color_scheme_notifications(true);
    let apply = {
        let ui = Arc::clone(ui);
        move |scheme: TerminalColorScheme| {
            let name = match scheme {
                TerminalColorScheme::Light => light,
                TerminalColorScheme::Dark => dark,
            };
            ui.push(UiCommand::ApplyThemeName(name));
            ui.render_handle.request_render();
        }
    };
    let tui = ui.ui.clone();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let scheme = detect_terminal_theme_for_auto(&tui, 100).await;
                apply(scheme);
            });
        }
        Err(_) => {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    runtime.block_on(async move {
                        let scheme = detect_terminal_theme_for_auto(&tui, 100).await;
                        apply(scheme);
                    });
                }
            });
        }
    }
}

/// Run an async block from a thread that may not be inside a Tokio runtime.
/// Selector and keybinding callbacks run on the TUI driver thread, which has
/// no runtime context (interactive_mode.rs module docs); a bare
/// `tokio::spawn` there panics with "there is no reactor running". Prefer the
/// ambient runtime when one is present; otherwise run the block on a
/// dedicated thread with a current-thread runtime (same fallback as
/// [`apply_auto_theme_pair`] above).
pub(crate) fn spawn_async<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(future);
        }
        Err(_) => {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    runtime.block_on(future);
                }
            });
        }
    }
}

/// Settings `on*Change` handlers (interactive-mode.ts:4171-4310). A free
/// function (not an inline closure) so tests can drive changes without
/// mounting the selector.
fn apply_settings_change(ui: &Arc<InteractiveUi>, change: SettingsChange) {
    let session = ui.session();
    match change {
        SettingsChange::AutoCompact(enabled) => {
            session.set_auto_compaction_enabled(enabled);
            lock(&ui.footer).set_auto_compact_enabled(enabled);
        }
        SettingsChange::ShowImages(enabled) => {
            session.settings_manager(|s| s.set_show_images(enabled));
            // TODO(unassigned): propagate to mounted ToolExecutionComponents
            // (interactive-mode.ts:4176-4182) — no v0.1 task claims this.
        }
        SettingsChange::ImageWidthCells(width) => {
            session.settings_manager(|s| s.set_image_width_cells(width));
            // TODO(unassigned): propagate to mounted ToolExecutionComponents
            // (as above).
        }
        SettingsChange::AutoResizeImages(enabled) => {
            session.settings_manager(|s| s.set_image_auto_resize(enabled));
        }
        SettingsChange::BlockImages(blocked) => {
            session.settings_manager(|s| s.set_block_images(blocked));
        }
        SettingsChange::EnableSkillCommands(enabled) => {
            session.settings_manager(|s| s.set_enable_skill_commands(enabled));
            // TODO(unassigned): re-run setupAutocompleteProvider
            // (interactive-mode.ts:4199) — no v0.1 task claims this.
        }
        SettingsChange::SteeringMode(mode) => {
            session.set_steering_mode(mode);
        }
        SettingsChange::FollowUpMode(mode) => {
            session.set_follow_up_mode(mode);
        }
        SettingsChange::Transport(transport) => {
            session.settings_manager(|s| s.set_transport(transport));
            // TODO(T13): session.agent.transport = transport.
        }
        SettingsChange::HttpIdleTimeoutMs(timeout_ms) => {
            session.settings_manager(|s| s.set_http_idle_timeout_ms(timeout_ms));
            // TODO(T13): configureHttpDispatcher(timeout_ms) — the HTTP
            // dispatcher is provider-layer plumbing (app.rs header).
            ui.show_status(&format!(
                "HTTP idle timeout: {}",
                format_http_idle_timeout_ms(timeout_ms)
            ));
        }
        SettingsChange::ThinkingLevel(level) => {
            session.set_thinking_level(setting_to_model_thinking(level));
            ui.update_editor_border_color();
        }
        SettingsChange::Theme(theme_setting) => {
            session.settings_manager(|s| s.set_theme(&theme_setting));
            // `applyFromSettings` (theme-controller.ts:37-60): automatic
            // pairs resolve against the terminal appearance (async); plain
            // names load directly. Both apply through the drain
            // (`ApplyThemeName`) so `apply_theme` never runs inside this
            // component callback (lock contract).
            match auto_theme_pair(Some(theme_setting.as_str())) {
                Some((light, dark)) => apply_auto_theme_pair(ui, light, dark),
                None => {
                    // `setAutoSync(false)` (theme-controller.ts:47).
                    ui.ui.set_terminal_color_scheme_notifications(false);
                    ui.push(UiCommand::ApplyThemeName(theme_setting));
                }
            }
        }
        SettingsChange::ThemePreview(_) => {
            // Ignored by the integration layer (component header).
        }
        SettingsChange::HideThinkingBlock(hidden) => {
            session.settings_manager(|s| s.set_hide_thinking_block(hidden));
            *lock(&ui.hide_thinking_block) = hidden;
            // interactive-mode.ts:4229-4235. The upstream per-child
            // `setHideThinkingBlock` loop is subsumed by the rebuild: new
            // components read the flag at construction.
            ui.rebuild_chat_from_messages();
        }
        SettingsChange::ShowCacheMissNotices(shown) => {
            session.settings_manager(|s| s.set_show_cache_miss_notices(shown));
            // `rebuildChatFromMessages` (interactive-mode.ts:4237-4240).
            ui.rebuild_chat_from_messages();
        }
        SettingsChange::CollapseChangelog(collapsed) => {
            session.settings_manager(|s| s.set_collapse_changelog(collapsed));
        }
        SettingsChange::EnableInstallTelemetry(enabled) => {
            session.settings_manager(|s| s.set_enable_install_telemetry(enabled));
        }
        SettingsChange::DoubleEscapeAction(action) => {
            session.settings_manager(|s| s.set_double_escape_action(action));
        }
        SettingsChange::TreeFilterMode(mode) => {
            session.settings_manager(|s| s.set_tree_filter_mode(mode));
        }
        SettingsChange::ShowHardwareCursor(enabled) => {
            session.settings_manager(|s| s.set_show_hardware_cursor(enabled));
            ui.ui.set_show_hardware_cursor(enabled);
        }
        SettingsChange::EditorPaddingX(padding) => {
            session.settings_manager(|s| s.set_editor_padding_x(padding));
            // `setPaddingX` (interactive-mode.ts:4263-4269): the custom
            // editor is the only editor; clamped like construction.
            lock(&ui.editor).set_padding_x(padding.min(128) as usize);
        }
        SettingsChange::OutputPad(padding) => {
            session.settings_manager(|s| s.set_output_pad(padding));
            ui.output_pad.store(padding as usize, Ordering::Relaxed);
            // interactive-mode.ts:4270-4290: while streaming, update the
            // live components in place; otherwise rebuild the chat.
            let streaming = lock(&ui.streaming);
            if streaming.is_some() || session.is_streaming() {
                if let Some(track) = streaming.as_ref() {
                    lock(&track.handle).set_output_pad(padding as usize);
                }
                // Deviation: historical chat children keep their padding —
                // the port has no component downcast for the upstream
                // `chatContainer.children` walk (interactive-mode.ts:4274-4282).
            } else {
                drop(streaming);
                ui.rebuild_chat_from_messages();
            }
        }
        SettingsChange::AutocompleteMaxVisible(max_visible) => {
            session.settings_manager(|s| s.set_autocomplete_max_visible(max_visible));
            // `setAutocompleteMaxVisible` (interactive-mode.ts:4291-4297);
            // clamped like construction.
            lock(&ui.editor).set_autocomplete_max_visible(max_visible.min(20) as usize);
        }
        SettingsChange::QuietStartup(enabled) => {
            session.settings_manager(|s| s.set_quiet_startup(enabled));
        }
        SettingsChange::DefaultProjectTrust(default_project_trust) => {
            session.settings_manager(|s| s.set_default_project_trust(default_project_trust));
        }
        SettingsChange::ClearOnShrink(enabled) => {
            session.settings_manager(|s| s.set_clear_on_shrink(enabled));
            ui.ui.set_clear_on_shrink(enabled);
        }
        SettingsChange::ShowTerminalProgress(enabled) => {
            session.settings_manager(|s| s.set_show_terminal_progress(enabled));
        }
        SettingsChange::Warnings(warnings) => {
            session.settings_manager(|s| s.set_warnings(&warnings));
        }
    }
    ui.render_handle.request_render();
}

/// The interactive-mode [`LlamaHost`]: `ctx.ui.notify` → the status line,
/// `ctx.modelRegistry.*` → the session's model runtime.
struct InteractiveLlamaHost {
    ui: Arc<InteractiveUi>,
}

#[async_trait::async_trait]
impl LlamaHost for InteractiveLlamaHost {
    fn mode_is_tui(&self) -> bool {
        true
    }

    fn notify(&self, message: &str, level: NotifyLevel) {
        match level {
            NotifyLevel::Info => self.ui.show_status(message),
            NotifyLevel::Warning => self.ui.show_warning(message),
            NotifyLevel::Error => self.ui.show_error(message),
        }
    }

    async fn provider_auth(&self) -> Result<Option<pir_ai::auth::AuthResult>, ModelsError> {
        self.ui
            .session()
            .model_runtime()
            .get_provider_auth(crate::extensions::llama::LLAMA_PROVIDER_ID)
            .await
    }

    async fn refresh_models(&self) {
        self.ui.session().model_runtime().refresh(None).await;
    }
}

/// `findLoginProviderOptions` (interactive-mode.ts:4888-4899): exact,
/// case-insensitive match on provider id or name.
fn find_login_provider_options(
    runtime: &ModelRuntime,
    provider_ref: &str,
) -> Vec<AuthSelectorProvider> {
    let normalized = provider_ref.trim().to_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }
    runtime
        .get_providers()
        .iter()
        .flat_map(|provider| AuthSelectorProvider::from_provider(provider, None))
        .filter(|provider| {
            provider.id.to_lowercase() == normalized || provider.name.to_lowercase() == normalized
        })
        .collect()
}

/// `startProviderLogin` (interactive-mode.ts:4925-4933).
fn start_provider_login(ui: &Arc<InteractiveUi>, provider: &AuthSelectorProvider) {
    match provider.auth_type {
        AuthType::Oauth => {
            // `showLoginDialog` (interactive-mode.ts:5286-5312) is a
            // T15 / OAuth-wave hook; the flow stays stubbed.
            ui.show_status(&format!(
                "Provider login is not available yet (T13): {}",
                provider.id
            ));
        }
        AuthType::ApiKey if provider.method_login => {
            show_api_key_login_dialog(ui, provider);
        }
        AuthType::ApiKey => {
            show_ambient_auth_dialog(ui, provider);
        }
    }
}

/// `LoginDialogComponent` wired to its `AuthInteraction` adapter: the
/// dialog callbacks resolve the adapter's pending prompt channel, and
/// cancel aborts the flow token (upstream `dialog.signal` +
/// `inputResolver`/`inputRejecter`, login-dialog.ts:16-17, 73-91).
struct LoginDialogInteraction {
    dialog: Arc<Mutex<LoginDialogComponent>>,
    state: Arc<LoginDialogInteractionState>,
}

/// Shared prompt/cancel state between the mounted dialog and the
/// adapter.
struct LoginDialogInteractionState {
    pending: Mutex<Option<oneshot::Sender<Result<String, ModelsError>>>>,
    signal: CancellationToken,
}

impl LoginDialogInteractionState {
    fn resolve(&self, result: Result<String, ModelsError>) {
        if let Some(sender) = lock(&self.pending).take() {
            let _ = sender.send(result);
        }
    }
}

/// `new Error("Login cancelled")` (login-dialog.ts:86, interactive-mode.ts
/// :5198): the exact message gates silent cancellation in the UI.
fn login_cancelled() -> ModelsError {
    ModelsError::new(ModelsErrorCode::Auth, "Login cancelled")
}

/// `new LoginDialogComponent(...)` (login-dialog.ts:30-71) plus the
/// `loginProvider` interaction wiring (interactive-mode.ts:5274-5284).
fn login_dialog_with_interaction(
    ui: &Arc<InteractiveUi>,
    provider_id: &str,
    provider_name: &str,
) -> (Arc<Mutex<LoginDialogComponent>>, LoginDialogInteraction) {
    let theme = Arc::clone(&lock(&ui.theme));
    let state = Arc::new(LoginDialogInteractionState {
        pending: Mutex::new(None),
        signal: CancellationToken::new(),
    });
    let dialog = Arc::new(Mutex::new(LoginDialogComponent::new(
        theme,
        provider_id,
        Some(provider_name),
        None,
    )));
    {
        let mut dialog_guard = lock(&dialog);
        let state_confirm = Arc::clone(&state);
        dialog_guard.on_prompt_confirm = Some(Box::new(move |value| {
            state_confirm.resolve(Ok(value.to_string()));
        }));
        let state_manual = Arc::clone(&state);
        dialog_guard.on_submit_manual = Some(Box::new(move |value| {
            state_manual.resolve(Ok(value.to_string()));
        }));
        let state_select = Arc::clone(&state);
        dialog_guard.on_select = Some(Box::new(move |id| {
            state_select.resolve(Ok(id.to_string()));
        }));
        let state_cancel = Arc::clone(&state);
        dialog_guard.on_cancel = Some(Box::new(move || {
            // `cancel()` (login-dialog.ts:83-91): abort the flow signal
            // and reject the pending prompt.
            state_cancel.signal.cancel();
            state_cancel.resolve(Err(login_cancelled()));
        }));
    }
    let interaction = LoginDialogInteraction {
        dialog: dialog.clone(),
        state,
    };
    (dialog, interaction)
}

/// Mount the dialog in place of the editor (upstream
/// `editorContainer.clear()` + `addChild(dialog)` + `setFocus(dialog)`,
/// interactive-mode.ts:5179-5182).
fn mount_login_dialog(ui: &Arc<InteractiveUi>, dialog: &Arc<Mutex<LoginDialogComponent>>) {
    let entry = shared_component_from_boxed(Box::new(FocusableRegion(dialog.clone())));
    ui.show_selector(entry);
}

impl AuthInteraction for LoginDialogInteraction {
    /// `dialog.signal` (login-dialog.ts:73-75).
    fn signal(&self) -> Option<CancellationToken> {
        Some(self.state.signal.clone())
    }

    /// `showAuthPrompt` (interactive-mode.ts:5237-5259): switch the
    /// dialog into the prompt mode and await the user's submit/cancel,
    /// racing the per-prompt signal when one is attached.
    fn prompt<'a>(&'a self, prompt: AuthPrompt) -> BoxFutureSend<'a, Result<String, ModelsError>> {
        let dialog = self.dialog.clone();
        let state = self.state.clone();
        Box::pin(async move {
            match &prompt {
                AuthPrompt::Text {
                    message,
                    placeholder,
                    ..
                }
                | AuthPrompt::Secret {
                    message,
                    placeholder,
                    ..
                } => {
                    lock(&dialog).show_prompt(message, placeholder.as_deref());
                }
                AuthPrompt::ManualCode { message, .. } => {
                    lock(&dialog).show_manual_input(message);
                }
                AuthPrompt::Select {
                    message, options, ..
                } => {
                    lock(&dialog).show_select(message, options.clone());
                }
            }
            // Register the pending channel before releasing the dialog
            // lock so a fast Enter cannot be dropped.
            let (tx, rx) = oneshot::channel();
            *lock(&state.pending) = Some(tx);

            let response = async {
                match rx.await {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(login_cancelled()),
                }
            };
            match prompt.signal() {
                None => response.await,
                Some(signal) => {
                    if signal.is_cancelled() {
                        Err(login_cancelled())
                    } else {
                        tokio::select! {
                            result = response => result,
                            _ = signal.cancelled() => Err(login_cancelled()),
                        }
                    }
                }
            }
        })
    }

    /// `notifyAuthDialog` (interactive-mode.ts:5261-5272).
    fn notify(&self, event: AuthEvent) {
        let mut dialog = lock(&self.dialog);
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                dialog.show_auth(&url, instructions.as_deref());
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                // Upstream calls showDeviceCode + showWaiting
                // (interactive-mode.ts:5264-5266); the port folds the
                // waiting line into the device-code state.
                dialog.show_device_code(
                    &user_code,
                    &verification_uri,
                    Some("Waiting for authentication..."),
                );
            }
            AuthEvent::Info { message, links } => {
                dialog.show_info(&message, links.unwrap_or_default(), false);
            }
            AuthEvent::Progress { message } => {
                dialog.show_progress(&message);
            }
        }
    }
}

/// `showApiKeyLoginDialog` (interactive-mode.ts:5159-5202): mount the
/// login dialog, run `ModelRuntime::login` through the dialog adapter,
/// then complete the authentication. Cancellation ("Login cancelled")
/// restores the editor without an error line (interactive-mode.ts:5198-
/// 5199).
fn show_api_key_login_dialog(ui: &Arc<InteractiveUi>, provider: &AuthSelectorProvider) {
    let previous_model = ui.session().model();
    let (dialog, interaction) = login_dialog_with_interaction(ui, &provider.id, &provider.name);
    mount_login_dialog(ui, &dialog);

    let ui = Arc::clone(ui);
    let provider_id = provider.id.clone();
    let provider_name = provider.name.clone();
    spawn_async(async move {
        let runtime = ui.session().model_runtime().clone();
        let result = runtime
            .login(&provider_id, AuthType::ApiKey, &interaction)
            .await;
        ui.hide_selector();
        match result {
            Ok(_credential) => {
                // Completion failures surface like login failures (upstream
                // `showApiKeyLoginDialog`'s catch, interactive-mode.ts:
                // 5193-5200).
                if let Err(error) = complete_provider_authentication(
                    &ui,
                    &provider_id,
                    &provider_name,
                    AuthType::ApiKey,
                    previous_model,
                )
                .await
                {
                    if error.message != "Login cancelled" {
                        ui.show_error(&format!(
                            "Failed to save API key for {provider_name}: {}",
                            error.message
                        ));
                    }
                }
            }
            Err(error) => {
                if error.message != "Login cancelled" {
                    ui.show_error(&format!(
                        "Failed to save API key for {provider_name}: {}",
                        error.message
                    ));
                }
            }
        }
    });
}

/// `showAmbientAuthDialog` (interactive-mode.ts:5136-5157): an
/// informational dialog for ambient-only providers (no interactive login
/// upstream). It stays mounted until the user closes it with Esc.
fn show_ambient_auth_dialog(ui: &Arc<InteractiveUi>, provider: &AuthSelectorProvider) {
    let theme = Arc::clone(&lock(&ui.theme));
    let dialog = Arc::new(Mutex::new(LoginDialogComponent::new(
        theme,
        &provider.id,
        Some(&provider.name),
        Some(&format!("{} setup", provider.name)),
    )));
    {
        let mut dialog_guard = lock(&dialog);
        let ui_restore = Arc::clone(ui);
        dialog_guard.on_cancel = Some(Box::new(move || {
            // `onComplete` → `restoreEditor`
            // (interactive-mode.ts:5146-5147, 5184-5189).
            ui_restore.hide_selector();
        }));
        dialog_guard.show_info(
            &format!(
                "{} is configured outside pi.",
                provider.method_name.as_deref().unwrap_or("Authentication")
            ),
            vec![],
            true,
        );
    }
    mount_login_dialog(ui, &dialog);
}

/// `completeProviderAuthentication` (interactive-mode.ts:5083-5134):
/// post-login status line, default-model auto-selection when the agent
/// still runs the "unknown" placeholder, and the footer/editor refresh.
/// `getAvailable` failures propagate to the caller, which reports them
/// like any login failure (upstream lets them reject out of
/// `showApiKeyLoginDialog`'s try).
async fn complete_provider_authentication(
    ui: &Arc<InteractiveUi>,
    provider_id: &str,
    provider_name: &str,
    auth_type: AuthType,
    previous_model: Option<Model>,
) -> Result<(), ModelsError> {
    let runtime = ui.session().model_runtime().clone();
    let _ = runtime.get_available(None).await?;
    let action_label = match auth_type {
        AuthType::Oauth => format!("Logged in to {provider_name}"),
        AuthType::ApiKey => format!("Saved API key for {provider_name}"),
    };

    let mut selected_model: Option<Model> = None;
    let mut selection_error: Option<String> = None;
    // The agent's "unknown" placeholder maps to `None`
    // (agent_session.rs `model_or_none`), matching upstream
    // `isUnknownModel` (interactive-mode.ts:220-222, 5095).
    if previous_model.is_none() {
        let available_models = runtime.get_available(None).await?;
        let provider_models: Vec<Model> = available_models
            .into_iter()
            .filter(|model| model.provider == provider_id)
            .collect();
        match default_model_for_provider(provider_id) {
            None => {
                selection_error = Some(format!(
                    "{action_label}, but no default model is configured for provider \"{provider_id}\". Use /model to select a model."
                ));
            }
            Some(_default_model_id) if provider_models.is_empty() => {
                selection_error = Some(format!(
                    "{action_label}, but no models are available for that provider. Use /model to select a model."
                ));
            }
            Some(default_model_id) => {
                let candidate = provider_models
                    .iter()
                    .find(|model| model.id == default_model_id)
                    .cloned();
                match candidate {
                    None => {
                        selection_error = Some(format!(
                            "{action_label}, but its default model \"{default_model_id}\" is not available. Use /model to select a model."
                        ));
                    }
                    Some(model) => match ui.session().set_model(model.clone()).await {
                        Ok(()) => selected_model = Some(model),
                        Err(error) => {
                            selected_model = None;
                            selection_error = Some(format!(
                                    "{action_label}, but selecting its default model failed: {error}. Use /model to select a model."
                                ));
                        }
                    },
                }
            }
        }
    }

    ui.update_available_provider_count();
    Component::invalidate(&mut *lock(&ui.footer));
    ui.update_editor_border_color();

    // `getAuthPath()` (config.ts:534-536): `{agentDir}/auth.json`.
    let auth_path = lock(&ui.session().resource_loader())
        .agent_dir()
        .join("auth.json");
    if let Some(selected) = &selected_model {
        ui.show_status(&format!(
            "{action_label}. Selected {}. Credentials saved to {}",
            selected.id,
            auth_path.display()
        ));
        // `maybeWarnAboutAnthropicSubscriptionAuth` /
        // `checkDaxnutsEasterEgg` (interactive-mode.ts:5124-5125) are
        // not ported (unassigned hooks).
    } else {
        ui.show_status(&format!(
            "{action_label}. Credentials saved to {}",
            auth_path.display()
        ));
        if let Some(selection_error) = selection_error {
            ui.show_error(&selection_error);
        }
    }
    Ok(())
}

/// `showLoginAuthTypeSelector(providerOptions?)` (interactive-mode.ts:
/// 4935-4991). With provider options (a `/login <ref>` match offering both
/// methods): pick the method for that provider. Without options (bare
/// `/login`): the auth-type pre-selector — both labels are always offered
/// upstream (`availableAuthTypes` defaults to both), and the choice filters
/// the provider selector.
fn show_login_auth_type_selector(
    ui: &Arc<InteractiveUi>,
    provider_options: Option<Vec<AuthSelectorProvider>>,
) {
    // pir collapses `method.loginLabel` (helpers.ts `lazyOAuth`) — no
    // provider exposes one yet (upstream oauthLoginLabel,
    // interactive-mode.ts:4937-4939).
    let subscription_label = "Sign in with an account";
    let api_key_label = "Sign in with an API key";
    let mut options: Vec<&str> = Vec::new();
    let has_oauth = provider_options
        .as_ref()
        .map(|providers| providers.iter().any(|p| p.auth_type == AuthType::Oauth))
        .unwrap_or(true);
    let has_api_key = provider_options
        .as_ref()
        .map(|providers| providers.iter().any(|p| p.auth_type == AuthType::ApiKey))
        .unwrap_or(true);
    if has_oauth {
        options.push(subscription_label);
    }
    if has_api_key {
        options.push(api_key_label);
    }
    if options.is_empty() {
        ui.show_status("No login methods available.");
        return;
    }
    if let Some(providers) = &provider_options {
        if options.len() == 1 {
            // Only one method: start it directly (interactive-mode.ts:4957-
            // 4962).
            if let Some(provider) = providers.first() {
                start_provider_login(ui, provider);
            }
            return;
        }
    }

    let title = match &provider_options {
        Some(providers) => match providers.first() {
            Some(provider) => format!("Select authentication method for {}:", provider.name),
            None => "Select authentication method:".to_owned(),
        },
        None => "Select authentication method:".to_owned(),
    };
    let options: Vec<String> = options.into_iter().map(str::to_string).collect();
    let select_ui = Arc::clone(ui);
    let selector = Arc::new(Mutex::new(ExtensionSelectorComponent::new(
        Arc::clone(&lock(&ui.theme)),
        Some(title),
        options.clone(),
        Box::new(move |option: Option<String>| {
            select_ui.hide_selector();
            let Some(option) = option else {
                return;
            };
            let auth_type = if option == subscription_label {
                AuthType::Oauth
            } else {
                AuthType::ApiKey
            };
            match &provider_options {
                Some(providers) => {
                    if let Some(provider) = providers.iter().find(|p| p.auth_type == auth_type) {
                        start_provider_login(&select_ui, provider);
                    }
                }
                // Bare `/login`: filter the provider list by the chosen
                // method (interactive-mode.ts:4983-4985).
                None => InteractiveUi::show_login_selector(&select_ui, Some(auth_type), None),
            }
        }),
        Box::new({
            let ui = Arc::clone(ui);
            move || {
                ui.hide_selector();
                ui.render_handle.request_render();
            }
        }),
        None,
    )));

    let entry = shared_component_from_boxed(Box::new(FocusableShell {
        inner: selector,
        focused: false,
    }));
    ui.show_selector(entry);
}
impl InteractiveUi {
    /// `showSettingsSelector` (interactive-mode.ts:4135-4319): mount the
    /// settings selector; `onChange` maps [`SettingsChange`] back to the
    /// settings/session setters.
    pub(crate) fn show_settings_selector(ui: &Arc<Self>) {
        let session = ui.session();
        let auto_compact = session.auto_compaction_enabled();
        let options = SettingsSelectorOptions {
            auto_compact,
            show_images: session.settings_manager(|s| s.get_show_images()),
            image_width_cells: session.settings_manager(|s| s.get_image_width_cells()),
            auto_resize_images: session.settings_manager(|s| s.get_image_auto_resize()),
            block_images: session.settings_manager(|s| s.get_block_images()),
            enable_skill_commands: session.settings_manager(|s| s.get_enable_skill_commands()),
            steering_mode: session.steering_mode(),
            follow_up_mode: session.follow_up_mode(),
            transport: session.settings_manager(|s| s.get_transport()),
            http_idle_timeout_ms: session
                .settings_manager(|s| s.get_http_idle_timeout_ms())
                .unwrap_or(crate::core::settings_manager::DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            thinking_level: model_thinking_to_setting(session.thinking_level()),
            available_thinking_levels: session
                .get_available_thinking_levels()
                .into_iter()
                .map(model_thinking_to_setting)
                .collect(),
            current_theme: session
                .settings_manager(|s| s.get_theme_setting())
                .unwrap_or_else(|| "dark".to_string()),
            // The terminal color scheme (upstream
            // themeController.getTerminalTheme) drives the theme submenu's
            // automatic-pair preview. Detection exists asynchronously
            // (`theme_watcher::detect_terminal_theme_for_auto`), but these
            // options are built synchronously, so default to dark.
            terminal_theme: TerminalColorScheme::Dark,
            available_themes: get_available_themes()
                .into_iter()
                .map(|info| info.name)
                .collect(),
            hide_thinking_block: *lock(&ui.hide_thinking_block),
            show_cache_miss_notices: session.settings_manager(|s| s.get_show_cache_miss_notices()),
            collapse_changelog: session.settings_manager(|s| s.get_collapse_changelog()),
            enable_install_telemetry: session
                .settings_manager(|s| s.get_enable_install_telemetry()),
            double_escape_action: session.settings_manager(|s| s.get_double_escape_action()),
            tree_filter_mode: session.settings_manager(|s| s.get_tree_filter_mode()),
            show_hardware_cursor: session.settings_manager(|s| s.get_show_hardware_cursor()),
            editor_padding_x: session.settings_manager(|s| s.get_editor_padding_x()),
            output_pad: session.settings_manager(|s| s.get_output_pad()),
            autocomplete_max_visible: session
                .settings_manager(|s| s.get_autocomplete_max_visible()),
            quiet_startup: session.settings_manager(|s| s.get_quiet_startup()),
            // The local `trust_manager::DefaultProjectTrust` mirrors the
            // settings enum (trust-manager.ts); the settings value is
            // already the type the selector expects.
            default_project_trust: session.settings_manager(|s| s.get_default_project_trust()),
            clear_on_shrink: session.settings_manager(|s| s.get_clear_on_shrink()),
            show_terminal_progress: session.settings_manager(|s| s.get_show_terminal_progress()),
            warnings: session.settings_manager(|s| s.get_warnings()),
        };

        let on_change = {
            let ui = Arc::clone(ui);
            Box::new(move |change: SettingsChange| apply_settings_change(&ui, change))
        };
        let on_cancel = {
            let ui = Arc::clone(ui);
            Box::new(move || {
                ui.hide_selector();
                ui.render_handle.request_render();
            })
        };

        let selector = Arc::new(Mutex::new(SettingsSelectorComponent::new(
            options,
            Arc::clone(&lock(&ui.theme)),
            on_change,
            on_cancel,
        )));
        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showModelSelector` (interactive-mode.ts:4454-4484).
    pub(crate) fn show_model_selector(ui: &Arc<Self>, initial_search_input: Option<String>) {
        let session = ui.session();
        let runtime = session.model_runtime().clone();
        let scoped_models: Vec<(Model, Option<ModelThinkingLevel>)> = session
            .scoped_models()
            .into_iter()
            .map(|scoped| (scoped.model, scoped.thinking_level))
            .collect();
        let current_model = session.model();

        let save_default = {
            let session = session.clone();
            Box::new(move |model: &Model| {
                session.settings_manager(|settings| {
                    settings.set_default_model_and_provider(&model.provider, &model.id);
                });
            })
        };
        let on_select = {
            let session = session.clone();
            let ui = Arc::clone(ui);
            Box::new(move |model: Model| {
                let session = session.clone();
                let ui = Arc::clone(&ui);
                spawn_async(async move {
                    // `onSelect` (interactive-mode.ts:4462-4474): hide the
                    // selector after the model is set.
                    match session.set_model(model.clone()).await {
                        Ok(()) => {
                            ui.hide_selector();
                            ui.update_editor_border_color();
                            ui.show_status(&format!("Model: {}", model.id));
                        }
                        Err(error) => ui.show_error(&error.raw_message()),
                    }
                });
            })
        };
        let on_cancel = {
            let ui = Arc::clone(ui);
            Box::new(move || {
                ui.hide_selector();
                ui.render_handle.request_render();
            })
        };

        let selector = Arc::new(Mutex::new(ModelSelectorComponent::new(
            current_model,
            runtime,
            scoped_models,
            Arc::clone(&lock(&ui.theme)),
            ui.ui.clone(),
            save_default,
            on_select,
            on_cancel,
            initial_search_input,
        )));
        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showModelsSelector` (interactive-mode.ts:4486-4574): the
    /// scoped-models selector.
    pub(crate) async fn show_models_selector(ui: &Arc<Self>) {
        let session = ui.session();
        let runtime = session.model_runtime().clone();
        // Get all available models (interactive-mode.ts:4488-4489).
        runtime.refresh(None).await;
        let all_models = runtime.get_available(None).await.unwrap_or_default();
        let all_model_ids: Vec<String> = all_models
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect();
        let configured_patterns = session.settings_manager(|s| s.get_enabled_models());
        let session_scoped_models = session.scoped_models();

        if all_models.is_empty()
            && configured_patterns
                .as_ref()
                .is_none_or(|patterns| patterns.is_empty())
            && session_scoped_models.is_empty()
        {
            ui.show_status("No models available");
            return;
        }

        // `currentEnabledIds` (interactive-mode.ts:4507-4522): session scope
        // wins; otherwise resolve the configured patterns.
        let mut current_enabled_ids: Option<Vec<String>> = None;
        if !session_scoped_models.is_empty() {
            current_enabled_ids = Some(
                session_scoped_models
                    .iter()
                    .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
                    .collect(),
            );
        } else if let Some(patterns) = configured_patterns.as_ref().filter(|p| !p.is_empty()) {
            let scope = resolve_model_scope_with_diagnostics(patterns, &runtime).await;
            let mut ids: Vec<String> = scope
                .scoped_models
                .iter()
                .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
                .collect();
            for diagnostic in &scope.diagnostics {
                if diagnostic.code == "no-match" && !ids.contains(&diagnostic.pattern) {
                    ids.push(diagnostic.pattern.clone());
                }
            }
            current_enabled_ids = Some(ids);
        }

        let on_change = {
            let session = session.clone();
            let ui = Arc::clone(ui);
            let all_model_ids = all_model_ids.clone();
            Box::new(move |enabled_ids: Option<Vec<String>>| {
                // `updateSessionModels` (interactive-mode.ts:4525-4544).
                let session = session.clone();
                let ui = Arc::clone(&ui);
                let runtime = session.model_runtime().clone();
                let all_model_ids = all_model_ids.clone();
                spawn_async(async move {
                    let has_enabled_available = enabled_ids
                        .as_ref()
                        .is_some_and(|ids| ids.iter().any(|id| all_model_ids.contains(id)));
                    let all_available_enabled = enabled_ids.as_ref().is_some_and(|ids| {
                        !ids.is_empty() && all_model_ids.iter().all(|id| ids.contains(id))
                    });
                    if let Some(ids) = &enabled_ids {
                        if has_enabled_available && !all_available_enabled {
                            let scope = resolve_model_scope_with_diagnostics(ids, &runtime).await;
                            session.set_scoped_models(scope.scoped_models);
                        } else {
                            // All enabled or none enabled = no filter.
                            session.set_scoped_models(Vec::new());
                        }
                    } else {
                        session.set_scoped_models(Vec::new());
                    }
                    ui.update_available_provider_count();
                    ui.render_handle.request_render();
                });
            })
        };
        let on_persist = {
            let session = session.clone();
            let ui = Arc::clone(ui);
            let all_model_ids = all_model_ids.clone();
            let all_models_len = all_models.len();
            Box::new(move |enabled_ids: Option<Vec<String>>| {
                // `onPersist` (interactive-mode.ts:4557-4564).
                let all_enabled = enabled_ids.as_ref().is_some_and(|ids| {
                    !ids.is_empty()
                        && ids.len() == all_models_len
                        && ids.iter().all(|id| all_model_ids.contains(id))
                });
                let new_patterns = if enabled_ids.is_none() || all_enabled {
                    None
                } else {
                    enabled_ids
                };
                session.settings_manager(|s| s.set_enabled_models(new_patterns));
                ui.show_status("Model selection saved to settings");
            })
        };
        let on_cancel = {
            let ui = Arc::clone(ui);
            Box::new(move || {
                ui.hide_selector();
                ui.render_handle.request_render();
            })
        };

        let selector = Arc::new(Mutex::new(ScopedModelsSelectorComponent::new(
            all_models,
            current_enabled_ids,
            Arc::clone(&lock(&ui.theme)),
            on_change,
            on_persist,
            on_cancel,
        )));
        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showSessionSelector` (interactive-mode.ts:4770-4806).
    pub(crate) fn show_session_selector(ui: &Arc<Self>) {
        let (cwd, session_dir, current_session_file) = {
            let manager = ui.session().session_manager();
            let manager = lock(&manager);
            let session_dir = if manager.uses_default_session_dir() {
                None
            } else {
                Some(manager.get_session_dir().to_path_buf())
            };
            (
                manager.get_cwd().to_path_buf(),
                session_dir,
                manager
                    .get_session_file()
                    .map(|path| path.to_string_lossy().into_owned()),
            )
        };

        let select_ui = Arc::clone(ui);
        let selector = Arc::new(Mutex::new(SessionSelectorComponent::new(
            cwd,
            session_dir,
            Arc::clone(&lock(&ui.theme)),
            ui.ui.clone(),
            Box::new(move |session_path: &str| {
                // `onSelect` (interactive-mode.ts:4779-4781): hide, then
                // resume on the run loop (the callback has no runtime
                // access).
                select_ui.hide_selector();
                let _ = select_ui
                    .input_tx
                    .send(EditorInput::ResumeSession(session_path.to_string()));
            }),
            Box::new({
                let ui = Arc::clone(ui);
                move || {
                    ui.hide_selector();
                    ui.render_handle.request_render();
                }
            }),
            // `onExit` (interactive-mode.ts:4788-4789): shutdown.
            Box::new({
                let ui = Arc::clone(ui);
                move || {
                    let _ = ui.shutdown_tx.send(true);
                }
            }),
            Some(Box::new({
                let ui = Arc::clone(ui);
                move |path: &str| {
                    // Delete the session file the way upstream
                    // `deleteSessionFile` does (session-selector.ts:645-680):
                    // prefer the `trash` CLI (recoverable); fall back to a
                    // plain unlink when it is unavailable.
                    let trashed = std::process::Command::new("trash")
                        .arg(path)
                        .status()
                        .map(|status| status.success())
                        .unwrap_or(false);
                    if trashed {
                        ui.show_status("Session moved to trash");
                        return;
                    }
                    match std::fs::remove_file(path) {
                        Ok(()) => ui.show_status("Session deleted"),
                        Err(error) => ui.show_error(&format!("Failed to delete session: {error}")),
                    }
                }
            })),
            Some(Box::new({
                let ui = Arc::clone(ui);
                move |path: &str, name: &str| {
                    // `renameSession` (interactive-mode.ts:4792-4797).
                    let next = name.trim();
                    if next.is_empty() {
                        return;
                    }
                    match SessionManager::open(Path::new(path), None, None) {
                        Ok(mut manager) => {
                            if let Err(error) = manager.append_session_info(next) {
                                ui.show_error(&format!("Failed to rename session: {error}"));
                            }
                        }
                        Err(error) => ui.show_error(&format!("Failed to rename session: {error}")),
                    }
                }
            })),
            current_session_file,
        )));

        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showTrustSelector` (interactive-mode.ts:4429-4452): the value strings
    /// built by [`build_trust_options`] encode the upstream
    /// `{ trusted, updates }` selection.
    pub(crate) fn show_trust_selector(ui: &Arc<Self>) {
        let manager = ui.session().session_manager();
        let cwd = lock(&manager).get_cwd().to_path_buf();
        let agent_dir = lock(&ui.session().resource_loader())
            .agent_dir()
            .to_path_buf();
        let store = ProjectTrustStore::new(&agent_dir);
        let options = build_trust_options(&cwd);

        let select_ui = Arc::clone(ui);
        let selector = Arc::new(Mutex::new(TrustSelectorComponent::new(
            Arc::clone(&lock(&ui.theme)),
            options,
            Box::new(move |value: &str| {
                // `onSelect` (interactive-mode.ts:4438-4444).
                let trusted = value.starts_with("trust");
                let result = match value {
                    "trust" => store.set(&cwd, Some(true)),
                    "untrust" => store.set(&cwd, Some(false)),
                    "trust-parent" => match cwd.parent() {
                        Some(parent) => store
                            .set_many(&[(parent.to_path_buf(), Some(true)), (cwd.clone(), None)]),
                        None => Ok(()),
                    },
                    // Session-only variants persist nothing (upstream
                    // `updates: []`, trust-manager.ts:60-63, 71-74).
                    _ => Ok(()),
                };
                select_ui.hide_selector();
                match result {
                    Ok(()) => select_ui.show_status(&format!(
                        "Saved trust decision: {}. Restart pir for this to take effect.",
                        if trusted { "trusted" } else { "untrusted" }
                    )),
                    Err(error) => {
                        select_ui.show_error(&format!("Failed to save trust decision: {error}"))
                    }
                }
            }),
            Box::new({
                let ui = Arc::clone(ui);
                move || {
                    ui.hide_selector();
                    ui.render_handle.request_render();
                }
            }),
        )));

        let entry = shared_component_from_boxed(Box::new(FocusableShell {
            inner: selector,
            focused: false,
        }));
        ui.show_selector(entry);
    }

    /// `showLoginProviderSelector` (interactive-mode.ts:4993-5034): the login
    /// provider list. Selecting a row dispatches through
    /// `startProviderLogin` — the api-key dialog flow, the ambient-info
    /// dialog, or the OAuth stub. `initial_search_input` mirrors the upstream
    /// `initialSearchInput` (pre-filled fuzzy search, e.g. the unmatched
    /// `/login <ref>` fallback).
    /// `showLoginProviderSelector(authType?, initialSearchInput?)`
    /// (interactive-mode.ts:4993-5034): the provider list, optionally
    /// filtered to one auth method (the bare `/login` pre-selector path).
    pub(crate) fn show_login_selector(
        ui: &Arc<Self>,
        auth_type: Option<AuthType>,
        initial_search_input: Option<String>,
    ) {
        let providers: Vec<AuthSelectorProvider> = ui
            .session()
            .model_runtime()
            .get_providers()
            .iter()
            .flat_map(|provider| AuthSelectorProvider::from_provider(provider, None))
            .filter(|provider| auth_type.is_none_or(|kind| provider.auth_type == kind))
            .collect();
        if providers.is_empty() {
            ui.show_status(match auth_type {
                Some(AuthType::Oauth) => "No subscription providers available.",
                Some(AuthType::ApiKey) => "No API key providers available.",
                None => "No login providers available.",
            });
            return;
        }

        let select_ui = Arc::clone(ui);
        let selector = Arc::new(Mutex::new(OAuthSelectorComponent::new(
            Arc::clone(&lock(&ui.theme)),
            AuthSelectorMode::Login,
            providers,
            Box::new(move |provider: &AuthSelectorProvider| {
                // `done()` then `startProviderLogin`
                // (interactive-mode.ts:5010-5020).
                select_ui.hide_selector();
                let provider = provider.clone();
                start_provider_login(&select_ui, &provider);
            }),
            Box::new({
                let ui = Arc::clone(ui);
                move || {
                    ui.hide_selector();
                    // Cancel from a method-filtered list returns to the
                    // auth-type pre-selector (interactive-mode.ts:5024-5029).
                    if auth_type.is_some() {
                        show_login_auth_type_selector(&ui, None);
                    } else {
                        ui.render_handle.request_render();
                    }
                }
            }),
            initial_search_input,
        )));

        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showOAuthSelector("logout")` (interactive-mode.ts:5036-5081): the
    /// stored-credential list. Selection removes the credential through
    /// `ModelRuntime::logout` (interactive-mode.ts:5063-5068).
    pub(crate) async fn show_logout_selector(ui: &Arc<Self>) {
        let runtime = ui.session().model_runtime().clone();
        let credentials = runtime.list_credentials().await.unwrap_or_default();
        if credentials.is_empty() {
            ui.show_status("No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.");
            return;
        }
        let mut providers: Vec<AuthSelectorProvider> = credentials
            .into_iter()
            .map(|credential| {
                let (auth_type, kind) = match credential.credential_type {
                    CredentialType::Oauth => (AuthType::Oauth, AuthType::Oauth),
                    CredentialType::ApiKey => (AuthType::ApiKey, AuthType::ApiKey),
                };
                AuthSelectorProvider {
                    id: credential.provider_id.clone(),
                    name: runtime
                        .get_provider(&credential.provider_id)
                        .map(|provider| provider.name().to_string())
                        .unwrap_or_else(|| credential.provider_id.clone()),
                    auth_type,
                    method_name: None,
                    // Logout rows do not run a login flow; the flag is
                    // unused here.
                    method_login: false,
                    status: Some(AuthCheck {
                        source: Some("stored credential".to_string()),
                        kind,
                    }),
                }
            })
            .collect();
        providers.sort_by(|a, b| a.name.cmp(&b.name));

        let select_ui = Arc::clone(ui);
        let selector = Arc::new(Mutex::new(OAuthSelectorComponent::new(
            Arc::clone(&lock(&ui.theme)),
            AuthSelectorMode::Logout,
            providers,
            Box::new(move |provider: &AuthSelectorProvider| {
                // `done()` first, then the async removal
                // (interactive-mode.ts:5054-5072).
                select_ui.hide_selector();
                let provider = provider.clone();
                let ui = Arc::clone(&select_ui);
                spawn_async(async move {
                    let runtime = ui.session().model_runtime().clone();
                    match runtime.logout(&provider.id).await {
                        Ok(()) => {
                            ui.update_available_provider_count();
                            let message = if provider.auth_type == AuthType::Oauth {
                                format!("Logged out of {}", provider.name)
                            } else {
                                format!(
                                    "Removed stored API key for {}. Environment variables and models.json config are unchanged.",
                                    provider.name
                                )
                            };
                            ui.show_status(&message);
                        }
                        Err(error) => ui.show_error(&format!("Logout failed: {}", error.message)),
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
            None,
        )));

        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    // =========================================================================
    // Built-in extension commands (extensions/index.ts builtInExtensions)
    // =========================================================================

    /// The llama.cpp extension's `/llama` handler (extensions/llama/index.ts
    /// `registerCommand("llama", …)`): mount the manager view in place of the
    /// editor and run the flow on a spawned task. `showLlamaUi`'s `done()`
    /// becomes `hide_selector` (ui.ts:480-492); flow errors notify like the
    /// upstream error boundary.
    pub(crate) fn handle_llama_command(ui: &Arc<Self>) {
        let (view, mut view_ui) =
            new_llama_view(Arc::clone(&lock(&ui.theme)), ui.render_handle.clone());
        ui.show_selector(shared_component_from_boxed(Box::new(FocusableRegion(
            Arc::new(Mutex::new(view)),
        ))));
        let ui = Arc::clone(ui);
        spawn_async(async move {
            let host = InteractiveLlamaHost {
                ui: Arc::clone(&ui),
            };
            // `findHuggingFaceToken()` (huggingface.ts:46-61) — process env +
            // the user's home directory.
            let env = crate::extensions::llama::huggingface::process_env();
            let home = crate::extensions::llama::huggingface::default_home_dir();
            let token = crate::extensions::llama::find_hugging_face_token(&env, &home).await;
            let hugging_face = crate::extensions::llama::HuggingFaceClient::new(
                token,
                crate::extensions::llama::DEFAULT_HUGGING_FACE_URL,
            );
            let result = crate::extensions::llama::run_llama_manager(
                &host,
                &crate::extensions::llama::shared_llama_provider(),
                &mut view_ui,
                hugging_face,
            )
            .await;
            ui.hide_selector();
            if let Err(error) = result {
                ui.show_error(&error.message);
            }
            ui.render_handle.request_render();
        });
    }

    // =========================================================================
    // Login flows (interactive-mode.ts:4888-5312)
    // =========================================================================

    /// `handleLoginCommand` (interactive-mode.ts:4901-4923): bare `/login`
    /// shows the auth-type pre-selector; a provider ref resolves exact
    /// (case-insensitive id/name) matches, single-hit starts the flow
    /// directly, a dual-method provider asks for the method, and a miss
    /// opens the provider selector with the ref pre-filled.
    pub(crate) async fn handle_login_command(&self, provider_ref: Option<&str>) {
        let Some(ui) = self.upgrade_self() else {
            return;
        };
        let runtime = ui.session().model_runtime().clone();
        // Upstream awaits getAvailable() first (interactive-mode.ts:4902);
        // errors surface through the later flow stages.
        let _ = runtime.get_available(None).await;

        let Some(provider_ref) = provider_ref.map(str::trim).filter(|r| !r.is_empty()) else {
            // Bare `/login`: the auth-type pre-selector first
            // (interactive-mode.ts:4903-4905).
            show_login_auth_type_selector(&ui, None);
            return;
        };

        let matches = find_login_provider_options(&runtime, provider_ref);
        if matches.len() == 1 {
            start_provider_login(&ui, &matches[0]);
            return;
        }
        if matches.len() > 1 {
            let mut ids = HashSet::new();
            for provider in &matches {
                ids.insert(provider.id.as_str());
            }
            if ids.len() == 1 {
                // One provider offering both auth methods
                // (interactive-mode.ts:4914-4919): pick the method.
                show_login_auth_type_selector(&ui, Some(matches.clone()));
                return;
            }
        }
        // No match / ambiguous ref: the provider list with the reference
        // pre-filled into the search (interactive-mode.ts:4922).
        Self::show_login_selector(&ui, None, Some(provider_ref.to_string()));
    }

    // =========================================================================
    // Command handlers (InteractiveUi half)
    // =========================================================================

    /// `handleModelCommand` (interactive-mode.ts:4321-4343).
    pub(crate) async fn handle_model_command(&self, search_term: &str) {
        let search_term = search_term.trim();
        let Some(ui) = self.upgrade_self() else {
            return;
        };
        if search_term.is_empty() {
            Self::show_model_selector(&ui, None);
            return;
        }

        // `getModelCandidates` (interactive-mode.ts:4350-4361): scoped models
        // win; otherwise refresh and list everything.
        let models: Vec<Model> = if self.session().scoped_models().is_empty() {
            self.session().model_runtime().refresh(None).await;
            self.session()
                .model_runtime()
                .get_available(None)
                .await
                .unwrap_or_default()
        } else {
            self.session()
                .scoped_models()
                .into_iter()
                .map(|scoped| scoped.model)
                .collect()
        };

        match find_exact_model_reference_match(search_term, &models) {
            Some(model) => match self.session().set_model(model.clone()).await {
                Ok(()) => {
                    self.update_editor_border_color();
                    self.show_status(&format!("Model: {}", model.id));
                }
                Err(error) => self.show_error(&error.raw_message()),
            },
            None => Self::show_model_selector(&ui, Some(search_term.to_string())),
        }
    }

    /// `handleReloadCommand` (interactive-mode.ts:5318-5419), simplified: the
    /// reload box and theme controller are skipped; the resource loader and
    /// the keybinding globals are reloaded.
    pub(crate) fn handle_reload_command(&self) {
        if self.session().is_streaming() {
            self.show_warning("Wait for the current response to finish before reloading.");
            return;
        }
        if self.session().is_compacting() {
            self.show_warning("Wait for compaction to finish before reloading.");
            return;
        }
        // The reload box (interactive-mode.ts:5330-5356) is not ported; the
        // editor stays mounted (unassigned — no v0.1 task claims it).
        //
        // `session.reload()` (agent-session.ts:2600-2628) — the upstream
        // reload-box body (:5371): settings/resources reload + extension
        // host reload (factories re-run, factory cache generation bumped) +
        // tool-registry rebuild + session_start/resources_discover(reload)
        // events. Runs in the background so the TUI stays responsive.
        let session = self.session();
        let Some(ui) = self.upgrade_self() else {
            return;
        };
        let ui_weak = Arc::downgrade(&ui);
        spawn_async(async move {
            session.reload().await;
            // The host's extension set was replaced — re-resolve the
            // shortcut hook against the fresh registry (the init-time
            // snapshot would keep stale shortcuts alive and miss new
            // ones). Idempotent.
            if let Some(ui) = ui_weak.upgrade() {
                crate::modes::interactive::extension_shortcuts::install_extension_shortcuts(
                    &ui, &session,
                );
            }
        });
        // Re-install both keybinding globals (re-reads the config file,
        // keybindings.ts:354-357).
        install_global_keybindings();
        // Extension-command conflicts with built-ins
        // (interactive-mode.ts:530-543); the extension list is a T15 hook,
        // so this is currently always empty.
        for diagnostic in
            crate::modes::interactive::autocomplete::get_builtin_command_conflict_diagnostics(self)
        {
            self.show_warning(&diagnostic);
        }
        self.show_status(
            "Reloading keybindings, extensions, skills, prompts, themes, and context files",
        );
    }

    // =========================================================================
    // Bash mode
    // =========================================================================

    /// `handleBashCommand` (interactive-mode.ts:5931-6016). The component is
    /// mounted immediately (pending area while streaming, chat otherwise) and
    /// streamed through `on_chunk`.
    pub(crate) fn handle_bash_command(&self, command: &str, exclude_from_context: bool) {
        if self.session().is_bash_running() {
            self.show_warning("A bash command is already running. Press Esc to cancel it first.");
            return;
        }

        let component = Arc::new(Mutex::new(BashExecutionComponent::new(
            command.to_string(),
            self.render_handle.clone(),
            Arc::clone(&lock(&self.theme)),
            exclude_from_context,
        )));
        if self.session().is_streaming() {
            // Deferred while streaming (interactive-mode.ts:5977-5980).
            let entry = Box::new(SharedChild(Arc::clone(&component)));
            let address = child_address(&*entry);
            lock(&self.pending_messages_container).children.push(entry);
            lock(&self.pending_bash_components).push((address, Arc::clone(&component)));
        } else {
            lock(&self.chat_container)
                .children
                .push(Box::new(SharedChild(Arc::clone(&component))));
        }
        *lock(&self.bash_component) = Some(Arc::clone(&component));
        self.render_handle.request_render();

        // Own the command string: the spawned task outlives the method body.
        let command = command.to_string();
        let session = self.session();
        let ui = self.upgrade_self();
        let render_handle = self.render_handle.clone();
        spawn_async(async move {
            // `user_bash` extension interception (interactive-mode.ts:5931-5940):
            // the first non-empty handler result wins; a full `result`
            // replacement skips local execution entirely (:5942-5966).
            let runner = session.extension_runner();
            if runner.has_handlers("user_bash") {
                let cwd = session.cwd().to_owned();
                if let Some(result) = runner
                    .emit_user_bash(&command, exclude_from_context, &cwd)
                    .await
                {
                    if !result.output.is_empty() {
                        lock(&component).append_output(&result.output);
                    }
                    lock(&component).set_complete(
                        result.exit_code,
                        result.cancelled,
                        result.truncated.then(|| TruncationResult {
                            content: result.output.clone(),
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
                        result
                            .full_output_path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned()),
                    );
                    session.record_bash_result(&command, &result, exclude_from_context);
                    if let Some(ui) = &ui {
                        *lock(&ui.bash_component) = None;
                        ui.render_handle.request_render();
                    }
                    return;
                }
            }

            let on_chunk: BashChunkCallback = {
                let component = Arc::clone(&component);
                let render_handle = render_handle.clone();
                Box::new(move |delta: &str| {
                    lock(&component).append_output(delta);
                    render_handle.request_render();
                })
            };
            let result = session
                .execute_bash(
                    &command,
                    ExecuteBashOptions {
                        exclude_from_context,
                        id: None,
                        on_chunk: Some(on_chunk),
                    },
                )
                .await;
            match result {
                Ok(result) => {
                    lock(&component).set_complete(
                        result.exit_code,
                        result.cancelled,
                        result.truncated.then(|| TruncationResult {
                            content: result.output.clone(),
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
                        result
                            .full_output_path
                            .map(|path| path.to_string_lossy().into_owned()),
                    );
                    // Local execute_bash already records the result
                    // (agent-session.rs:2076); upstream records it explicitly
                    // (interactive-mode.ts:5967).
                }
                Err(error) => {
                    lock(&component).set_complete(None, false, None, None);
                    if let Some(ui) = &ui {
                        ui.show_error(&format!("Bash command failed: {}", error.raw_message()));
                    }
                }
            }
            if let Some(ui) = &ui {
                *lock(&ui.bash_component) = None;
                ui.render_handle.request_render();
            }
        });
    }

    /// `flushPendingBashComponents` (interactive-mode.ts:4106-4112): move
    /// pending bash components into the chat. Upstream calls it at the start
    /// of `handleSubmit` (interactive-mode.ts:2832); the local call site is
    /// the slash-command dispatch in `handle_submit`.
    pub(crate) fn flush_pending_bash_components(&self) {
        let pending = std::mem::take(&mut *lock(&self.pending_bash_components));
        if pending.is_empty() {
            return;
        }
        let mut pending_container = lock(&self.pending_messages_container);
        let mut chat = lock(&self.chat_container);
        for (address, component) in pending {
            remove_child_by_address(&mut pending_container, address);
            chat.children.push(Box::new(SharedChild(component)));
        }
        self.render_handle.request_render();
    }
}

// =============================================================================
// Runtime-dependent commands (InteractiveMode half)
// =============================================================================

impl InteractiveMode {
    /// `handleCloneCommand` (interactive-mode.ts:4614-4633).
    pub(crate) async fn handle_clone_command(&mut self) {
        let leaf_id = {
            let manager = self.session.session_manager();
            let manager = lock(&manager);
            manager.get_leaf_id().map(str::to_owned)
        };
        let Some(leaf_id) = leaf_id else {
            self.ui_state.show_status("Nothing to clone yet");
            return;
        };

        match self.runtime.fork(&leaf_id, ForkPosition::At, None).await {
            Ok(result) => {
                if result.cancelled {
                    self.ui_state.render_handle.request_render();
                    return;
                }
                self.rebind_session_ui().await;
                lock(&self.ui_state.editor).set_text("");
                self.ui_state.show_status("Cloned to new session");
            }
            Err(error) => self.ui_state.show_error(&error.raw_message()),
        }
    }

    /// The user-message selector's `onSelect` (interactive-mode.ts:4589-4602):
    /// `runtimeHost.fork(entryId)` (default position `Before` — the picked
    /// user message is dropped and its text returned) and refill the editor
    /// with the selected text.
    pub(crate) async fn handle_fork_command(&mut self, entry_id: &str) {
        match self
            .runtime
            .fork(entry_id, ForkPosition::Before, None)
            .await
        {
            Ok(result) => {
                if result.cancelled {
                    self.ui_state.render_handle.request_render();
                    return;
                }
                // Upstream rebinds via the runtime's `setRebindSession` hook
                // (interactive-mode.ts:455-457); the local hook stays `None`
                // and the handlers rebind explicitly (module header).
                self.rebind_session_ui().await;
                lock(&self.ui_state.editor).set_text(result.selected_text.as_deref().unwrap_or(""));
                self.ui_state.show_status("Forked to new session");
            }
            Err(error) => self.ui_state.show_error(&error.raw_message()),
        }
    }

    /// `handleClearCommand` for `/new` (interactive-mode.ts:5859-5876). The
    /// chat is cleared and re-rendered from the fresh session; upstream
    /// appends an accent "✓ New session started" row instead.
    pub(crate) async fn handle_new_command(&mut self) {
        match self.runtime.new_session(None, None, None).await {
            Ok(cancelled) => {
                if cancelled {
                    return;
                }
                self.rebind_session_ui().await;
                self.ui_state.show_status("✓ New session started");
            }
            Err(error) => self.ui_state.show_error(&error.raw_message()),
        }
    }

    /// `/resume` — with a session-file argument it switches sessions
    /// (`handleResumeSession`, interactive-mode.ts:4808-4843); without one it
    /// shows the session selector (interactive-mode.ts:2777-2780).
    pub(crate) async fn handle_resume_command(&mut self, args: &str) {
        let path = args.trim();
        if path.is_empty() {
            InteractiveUi::show_session_selector(&self.ui_state);
            return;
        }
        // ADR-0006/D-044 closure: cross-cwd resumes prompt via the TUI
        // trust selector (async bridge, T15 W7).
        let ui_weak = Arc::downgrade(&self.ui_state);
        let trust_factory = Arc::new(move |_cwd: &std::path::Path| {
            let ui_weak = ui_weak.clone();
            crate::core::trust_manager::ProjectTrustContext {
                has_ui: true,
                select: None,
                select_async: Some(Arc::new(move |title: String, options: Vec<String>| {
                    let ui_weak = ui_weak.clone();
                    Box::pin(async move {
                        use pir_ext_host::api::UiBridge;
                        let ui = ui_weak.upgrade()?;
                        super::ui_bridge::InteractiveUiBridge::new(&ui)
                            .select(&title, &options, None)
                            .await
                    }) as futures::future::BoxFuture<'static, Option<String>>
                })),
            }
        });
        match self
            .runtime
            .switch_session(path, None, None, Some(trust_factory))
            .await
        {
            Ok(cancelled) => {
                if cancelled {
                    return;
                }
                self.rebind_session_ui().await;
                self.ui_state.show_status("Resumed session");
            }
            Err(error) => self.ui_state.show_error(&error.raw_message()),
        }
    }

    /// `rebindCurrentSession({ renderBeforeBind: true })`
    /// (interactive-mode.ts:1732-1758): detach the old subscription, swap the
    /// bound session everywhere, rebuild the chat from the new session's
    /// entries and re-subscribe. Called by every session-switching command
    /// (`/new`, `/resume`, `/clone`, `/fork`, `/import`); the runtime's
    /// `setRebindSession` hook stays `None` (module header).
    pub(crate) async fn rebind_session_ui(&mut self) {
        let session = self.runtime.session().clone();
        self.session = session.clone();

        // Detach the old session's subscription first
        // (interactive-mode.ts:1734-1735).
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }

        self.ui_state.set_session(session.clone());
        self.ui_state.apply_runtime_settings(&session);

        // `renderCurrentSessionState` (interactive-mode.ts:1766-1776): drop
        // all per-session render state before re-rendering from the new
        // session's entries.
        lock(&self.ui_state.loaded_resources_container).clear();
        lock(&self.ui_state.chat_container).clear();
        lock(&self.ui_state.pending_messages_container).clear();
        lock(&self.ui_state.compaction_queue).clear();
        *lock(&self.ui_state.streaming) = None;
        lock(&self.ui_state.pending_tools).clear();
        self.ui_state.update_pending_messages_display();
        self.ui_state.render_initial_messages();

        // Subscribe AFTER rendering (upstream `renderBeforeBind: true`,
        // interactive-mode.ts:1739-1742) so no event is missed between the
        // rebuild and the subscription.
        self.unsubscribe = Some(self.ui_state.subscribe_to_agent());

        // `bindCurrentSessionExtensions` (interactive-mode.ts:1744).
        session
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

        self.ui_state.update_available_provider_count();
        self.ui_state.update_editor_border_color();
        self.ui_state.update_terminal_title();
    }
}

impl InteractiveUi {
    /// `applyRuntimeSettings` (interactive-mode.ts:1709-1730): re-apply the
    /// (possibly new project's) settings to the footer, the data provider
    /// and the editor after a session switch. `configureHttpDispatcher` has
    /// no local equivalent yet (TODO(T13) at the settings selector); the `!clearOnShrink` status-container clear is
    /// folded into the rebind's full render-state reset.
    fn apply_runtime_settings(&self, session: &AgentSession) {
        let manager = session.session_manager();
        let cwd = lock(&manager).get_cwd().to_path_buf();
        drop(manager);
        {
            let mut footer = lock(&self.footer);
            footer.set_session(session.clone());
            footer.set_auto_compact_enabled(session.auto_compaction_enabled());
        }
        self.footer_data.set_cwd(&cwd);
        let hide_thinking = session.settings_manager(|s| s.get_hide_thinking_block());
        *lock(&self.hide_thinking_block) = hide_thinking;
        self.output_pad.store(
            session.settings_manager(|s| s.get_output_pad()) as usize,
            Ordering::Relaxed,
        );
        self.ui
            .set_show_hardware_cursor(session.settings_manager(|s| s.get_show_hardware_cursor()));
        self.ui
            .set_clear_on_shrink(session.settings_manager(|s| s.get_clear_on_shrink()));
        {
            let padding_x = session
                .settings_manager(|s| s.get_editor_padding_x())
                .min(128);
            let max_visible = session
                .settings_manager(|s| s.get_autocomplete_max_visible())
                .min(20);
            let mut editor = lock(&self.editor);
            editor.set_padding_x(padding_x as usize);
            editor.set_autocomplete_max_visible(max_visible as usize);
        }
    }
}

// =============================================================================
// =============================================================================
// App-level keybinding actions (interactive-mode.ts setupKeyHandlers,
// L2561-2627) — wired in `InteractiveUi::setup_key_handlers`.
// =============================================================================

impl InteractiveUi {
    /// `cycleThinkingLevel` (interactive-mode.ts:3778-3787).
    pub(crate) fn cycle_thinking_level(&self) {
        let Some(new_level) = self.session().cycle_thinking_level() else {
            self.show_status("Current model does not support thinking");
            return;
        };
        Component::invalidate(&mut *lock(&self.footer));
        self.update_editor_border_color();
        self.show_status(&format!("Thinking level: {}", new_level.as_str()));
    }

    /// `cycleModel` (interactive-mode.ts:3789-3826) — spawned from the
    /// dispatch (the session call is async).
    pub(crate) fn cycle_model(&self, direction: CycleDirection) {
        let session = self.session();
        let Some(ui) = self.upgrade_self() else {
            return;
        };
        spawn_async(async move {
            let result = session.cycle_model(direction).await;
            match result {
                Ok(Some(result)) => {
                    Component::invalidate(&mut *lock(&ui.footer));
                    ui.update_editor_border_color();
                    let thinking_str =
                        if result.model.reasoning && result.thinking_level.as_str() != "off" {
                            format!(" (thinking: {})", result.thinking_level.as_str())
                        } else {
                            String::new()
                        };
                    ui.show_status(&format!(
                        "Switched to {}{thinking_str}",
                        if result.model.name.is_empty() {
                            result.model.id.as_str()
                        } else {
                            result.model.name.as_str()
                        }
                    ));
                }
                Ok(None) => {
                    let message = if !session.scoped_models().is_empty() {
                        "Only one model in scope"
                    } else {
                        "Only one model available"
                    };
                    ui.show_status(message);
                }
                Err(error) => ui.show_error(&error.raw_message()),
            }
        });
    }

    /// `handleDequeue` (interactive-mode.ts:3759-3766): restore all queued
    /// messages to the editor (merged across queues, same as upstream's
    /// `restoreQueuedMessagesToEditor()`).
    pub(crate) fn handle_dequeue(&self) {
        let restored = self.restore_queued_messages_to_editor(false);
        if restored == 0 {
            self.show_status("No queued messages to restore");
        } else {
            self.show_status(&format!(
                "Restored {restored} queued message{} to editor",
                if restored > 1 { "s" } else { "" }
            ));
        }
    }

    /// `handleCtrlZ` (interactive-mode.ts:3690-3725): stop the TUI, suspend
    /// the process group with SIGTSTP; SIGCONT restores the TUI (the signal
    /// handler lives on the run loop, `InteractiveMode::run`).
    pub(crate) fn handle_ctrl_z(&self) {
        #[cfg(unix)]
        {
            // Stop the TUI first so the terminal is usable while suspended.
            self.ui.stop();
            // Signal the whole process group (pid 0), like upstream
            // `process.kill(0, "SIGTSTP")`.
            // SAFETY: kill(0, SIGTSTP) is a process-group signal with no
            // memory access; the handler on the run loop restores the TUI on
            // SIGCONT.
            unsafe {
                libc::kill(0, libc::SIGTSTP);
            }
        }
        #[cfg(not(unix))]
        {
            self.show_status("Suspend to background is not supported on Windows");
        }
    }

    /// `handleOpenExternalEditor` (interactive-mode.ts:3846-3866) — the
    /// process-spawning flow lives in `external_editor.rs`
    /// ([`InteractiveUi::handle_open_external_editor_real`]).
    pub(crate) fn handle_open_external_editor(&self) {
        self.handle_open_external_editor_real();
    }

    /// `handleClipboardPaste` (interactive-mode.ts:2629-2652): image paste
    /// via the X11 clipboard (`utils/clipboard-image.ts`
    /// `readClipboardImageViaXclip`, simplified to the PNG probe), else a
    /// plain-text paste via the platform tools (`utils/clipboard.ts`
    /// `readClipboardText`, native addon replaced by xclip / wl-paste /
    /// pbpaste). Image paths are inserted by absolute path; the temp file is
    /// intentionally left in place (upstream never removes it either).
    /// Dispatch-side entry: deferred to the drain (insertion touches the
    /// editor, which is locked during the dispatch).
    pub(crate) fn handle_paste_image(&self) {
        self.handle_paste_image_impl();
    }

    /// `handleClipboardPaste` (interactive-mode.ts:2629-2652) — runs from
    /// the drain (UiCommand::PasteImage).
    pub(crate) fn handle_paste_image_impl(&self) {
        if let Some(bytes) = read_clipboard_image_png() {
            let file_path =
                std::env::temp_dir().join(format!("pi-clipboard-{}.png", clipboard_uuid()));
            if std::fs::write(&file_path, bytes).is_ok() {
                lock(&self.editor).insert_text_at_cursor(&file_path.display().to_string());
                self.render_handle.request_render();
                return;
            }
            self.show_status("Failed to write clipboard image");
            return;
        }

        match read_clipboard_text() {
            Some(text) if !text.is_empty() => {
                lock(&self.editor).insert_text_at_cursor(&text);
                self.render_handle.request_render();
            }
            _ => self.show_status(
                "Clipboard is empty or no clipboard tool found (xclip / wl-paste / pbpaste)",
            ),
        }
    }

    /// `app.session.toggleNamedFilter` — acts inside the session selector;
    /// outside it there is nothing to toggle.
    pub(crate) fn handle_toggle_named_filter(&self) {
        self.show_status("Open the session selector to use the named filter");
    }
}

// -----------------------------------------------------------------------------
// Clipboard helpers (`utils/clipboard-image.ts` / `utils/clipboard.ts`)
// -----------------------------------------------------------------------------

/// Upstream `DEFAULT_READ_TIMEOUT_MS` (clipboard-image.ts:18) — caps how
/// long a clipboard read may block (xclip `-o` waits for the selection
/// owner, which can hang on an empty clipboard).
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_millis(3000);

/// Run a command capturing stdout, bounded by `timeout` (upstream
/// `spawnSync` `timeout`/`maxBuffer` options, clipboard-image.ts:89-116).
/// A reader thread drains the pipe so large payloads cannot deadlock the
/// child against the buffer. `None` = spawn failure, timeout or non-zero
/// exit (upstream `ok: false`).
fn run_clipboard_command(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    status.filter(|status| status.success()).map(|_| bytes)
}

/// Probe the X11 clipboard for a PNG image (`readClipboardImageViaXclip`,
/// clipboard-image.ts:213-238, collapsed to the preferred `image/png`
/// target). `None` when xclip is absent, times out, or holds no image.
fn read_clipboard_image_png() -> Option<Vec<u8>> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let bytes = run_clipboard_command(
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
        CLIPBOARD_READ_TIMEOUT,
    )?;
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

/// `readClipboardText` (clipboard.ts:36-47) — the native clipboard addon is
/// replaced by the platform tools: `wl-paste` on Wayland, `xclip` on X11,
/// `pbpaste` on macOS.
fn read_clipboard_text() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|session| session == "wayland")
                .unwrap_or(false);
        let bytes = if is_wayland {
            run_clipboard_command("wl-paste", &[], CLIPBOARD_READ_TIMEOUT).or_else(|| {
                run_clipboard_command(
                    "xclip",
                    &["-selection", "clipboard", "-o"],
                    CLIPBOARD_READ_TIMEOUT,
                )
            })
        } else {
            run_clipboard_command(
                "xclip",
                &["-selection", "clipboard", "-o"],
                CLIPBOARD_READ_TIMEOUT,
            )
        };
        bytes.and_then(|bytes| String::from_utf8(bytes).ok())
    }
    #[cfg(target_os = "macos")]
    {
        run_clipboard_command("pbpaste", &[], CLIPBOARD_READ_TIMEOUT)
            .and_then(|bytes| String::from_utf8(bytes).ok())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// `crypto.randomUUID()` stand-in: monotonic time + per-process counter (no
/// `uuid` dependency in the workspace).
fn clipboard_uuid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use pir_agent::messages::AgentMessage;
    use pir_ai::types::{UserContent, UserMessage, UserRole};

    use super::*;
    use crate::core::agent_session::AgentSession;
    use crate::core::agent_session_runtime::{
        AgentSessionRuntime, CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
    };
    use crate::core::agent_session_services::{
        create_agent_session_services, CreateAgentSessionServicesOptions,
    };
    use crate::core::model_resolver::find_exact_model_reference_match;
    use crate::core::session_manager::NewSessionOptions;
    use crate::modes::interactive::interactive_mode::InteractiveModeOptions;
    use crate::modes::interactive::test_support::{
        build_test_session, TempDir, TestSession, TestTerminal,
    };
    use crate::sdk::{create_agent_session, CreateAgentSessionOptions, NoTools};

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(text.to_string()),
            timestamp: 1_700_000_000_000,
        })
    }

    /// `mode_harness` (interactive-mode tests): the `TestSession` is
    /// destructured so the temp dir outlives the harness (bash execution and
    /// the trust store need a live cwd / agent dir).
    async fn mode_harness() -> (InteractiveMode, Arc<TestTerminal>, AgentSession, TempDir) {
        install_global_keybindings();
        let harness = build_test_session().await;
        let TestSession {
            _tmp,
            runtime,
            session,
            cwd: _,
        } = harness;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        (mode, terminal, session, _tmp)
    }

    /// Like [`mode_harness`] but with a working session factory, so the
    /// runtime-dependent commands (`/new`, `/resume`, `/clone`) can switch
    /// sessions.
    async fn switchable_harness() -> (InteractiveMode, Arc<TestTerminal>, AgentSession, TempDir) {
        install_global_keybindings();
        let tmp = TempDir::new();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(
            agent_dir.join("models.json"),
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "PIR_TEST_INTERACTIVE_KEY",
                "models": [{"id": "m1", "contextWindow": 200000}]
            }}}"#,
        )
        .expect("write models.json");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).expect("cwd dir");

        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: None,
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        })
        .await
        .expect("create services");

        let model = services
            .model_runtime
            .get_model("custom", "m1")
            .expect("test model must compose");

        let session_manager = Arc::new(Mutex::new(
            SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
                .expect("in-memory session manager"),
        ));

        let created = create_agent_session(CreateAgentSessionOptions {
            cwd: Some(cwd.clone()),
            agent_dir: Some(agent_dir.clone()),
            model: Some(model),
            no_tools: Some(NoTools::All),
            services: Some(services.clone()),
            session_manager: Some(session_manager),
            ..Default::default()
        })
        .await
        .expect("create test session");

        let factory = {
            use crate::PirError;
            use futures::future::BoxFuture;
            let services = services.clone();
            let cwd = cwd.clone();
            let agent_dir = agent_dir.clone();
            Arc::new(move |options: CreateRuntimeOptions| {
                let services = services.clone();
                let cwd = cwd.clone();
                let agent_dir = agent_dir.clone();
                Box::pin(async move {
                    let created = create_agent_session(CreateAgentSessionOptions {
                        cwd: Some(cwd),
                        agent_dir: Some(agent_dir),
                        model: None,
                        no_tools: Some(NoTools::All),
                        services: Some(services),
                        session_manager: Some(options.session_manager),
                        ..Default::default()
                    })
                    .await?;
                    Ok(CreateAgentSessionRuntimeResult {
                        session: created.session,
                        services: created.services.expect("services round-trip"),
                        diagnostics: Vec::new(),
                        model_fallback_message: created.model_fallback_message,
                    })
                })
                    as BoxFuture<'static, Result<CreateAgentSessionRuntimeResult, PirError>>
            })
        };

        let runtime =
            AgentSessionRuntime::new(created.session.clone(), services, factory, Vec::new(), None);
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        (mode, terminal, created.session, tmp)
    }

    /// The fixture models.json declares `apiKey` for the custom provider,
    /// but runtime auth resolution is env/credential-driven; inject a
    /// runtime API key so `get_available` marks `custom/m1` as available.
    async fn make_model_available(session: &AgentSession) {
        session
            .model_runtime()
            .set_runtime_api_key("custom", "sk-test-interactive")
            .await;
    }

    fn rendered_chat(ui: &InteractiveUi) -> String {
        lock(&ui.chat_container).render(60).join("\n")
    }

    fn selector_mounted(ui: &InteractiveUi) -> bool {
        lock(&ui.active_selector).is_some()
    }

    // ---------------------------------------------------------------------
    // Selectors
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn show_settings_selector_mounts() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        InteractiveUi::show_settings_selector(&mode.ui_state);
        assert!(
            selector_mounted(&mode.ui_state),
            "settings selector mounted"
        );
        let rendered = lock(&mode.ui_state.active_selector)
            .as_ref()
            .map(|entry| entry.lock().unwrap().render(60).join("\n"))
            .unwrap_or_default();
        assert!(rendered.contains("Auto-compact"), "rendered: {rendered}");
    }

    #[tokio::test]
    async fn show_model_selector_mounts() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        InteractiveUi::show_model_selector(&mode.ui_state, None);
        assert!(selector_mounted(&mode.ui_state), "model selector mounted");
    }

    #[tokio::test]
    async fn show_models_selector_mounts_with_available_models() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        make_model_available(&session).await;
        InteractiveUi::show_models_selector(&mode.ui_state).await;
        assert!(
            selector_mounted(&mode.ui_state),
            "scoped-models selector mounted; chat: {}",
            rendered_chat(&mode.ui_state)
        );
    }

    #[tokio::test]
    async fn show_session_selector_mounts() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        InteractiveUi::show_session_selector(&mode.ui_state);
        assert!(selector_mounted(&mode.ui_state), "session selector mounted");
    }

    #[tokio::test]
    async fn trust_selector_saves_decision() {
        let (mut mode, terminal, _session, _tmp) = mode_harness().await;
        // The driver must be running to dispatch input: `on_select` →
        // `hide_selector` mutates the TUI, which is only safe when the
        // dispatch holds the inner Tui lock (UI ops defer through
        // `run_or_queue`; a direct lock-and-dispatch re-locks the focused
        // component in `set_focus` and deadlocks).
        mode.init().await;
        InteractiveUi::show_trust_selector(&mode.ui_state);
        assert!(selector_mounted(&mode.ui_state));

        // Confirm the highlighted option ("Trust") through the driver.
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());

        assert!(
            !selector_mounted(&mode.ui_state),
            "selector hidden after selection"
        );
        let chat = rendered_chat(&mode.ui_state);
        assert!(
            chat.contains("Saved trust decision: trusted"),
            "chat: {chat}"
        );

        // The decision landed in the trust store.
        let cwd = lock(&mode.ui_state.session().session_manager())
            .get_cwd()
            .to_path_buf();
        let agent_dir = lock(&mode.ui_state.session().resource_loader())
            .agent_dir()
            .to_path_buf();
        let store = ProjectTrustStore::new(&agent_dir);
        assert_eq!(store.get(&cwd).unwrap(), Some(true));
    }

    #[tokio::test]
    async fn login_selector_mounts_with_providers() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        InteractiveUi::show_login_selector(&mode.ui_state, None, None);
        assert!(
            selector_mounted(&mode.ui_state),
            "login selector mounted; chat: {}",
            rendered_chat(&mode.ui_state)
        );
    }

    #[tokio::test]
    async fn logout_selector_shows_status_when_no_credentials() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        InteractiveUi::show_logout_selector(&mode.ui_state).await;
        assert!(!selector_mounted(&mode.ui_state));
        let chat = rendered_chat(&mode.ui_state);
        assert!(
            chat.contains("No stored credentials to remove"),
            "chat: {chat}"
        );
    }

    // ---------------------------------------------------------------------
    // Command handlers
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn handle_model_command_empty_opens_selector() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        mode.ui_state.handle_model_command("  ").await;
        assert!(selector_mounted(&mode.ui_state), "model selector mounted");
    }

    #[tokio::test]
    async fn handle_model_command_exact_match_sets_model() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        make_model_available(&session).await;
        mode.ui_state.handle_model_command("custom/m1").await;
        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Model: m1"), "chat: {chat}");
        assert_eq!(
            mode.ui_state.session().model().map(|model| model.id),
            Some("m1".to_string())
        );
    }

    #[tokio::test]
    async fn handle_reload_command_shows_status() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        mode.ui_state.handle_reload_command();
        let chat = rendered_chat(&mode.ui_state);
        // The dim status line wraps at the 60-cell render width. The reload
        // itself runs in the background (session.reload is async).
        assert!(
            chat.contains("Reloading keybindings, extensions, skills, prompts,"),
            "chat: {chat}"
        );
    }

    #[tokio::test]
    async fn handle_clone_command_forks_leaf() {
        let (mut mode, _terminal, session, _tmp) = switchable_harness().await;
        // Give the session a leaf to fork from.
        lock(&session.session_manager())
            .append_message(user_message("fork me"))
            .expect("append message");
        let old_session_id = mode.session.session_id();
        mode.handle_clone_command().await;
        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Cloned to new session"), "chat: {chat}");
        assert_ne!(mode.session.session_id(), old_session_id);
    }

    #[tokio::test]
    async fn handle_new_command_starts_fresh_session() {
        let (mut mode, _terminal, _session, _tmp) = switchable_harness().await;
        let old_session_id = mode.session.session_id();
        mode.handle_new_command().await;
        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("✓ New session started"), "chat: {chat}");
        assert_ne!(mode.session.session_id(), old_session_id);
    }

    #[tokio::test]
    async fn handle_resume_command_without_args_shows_selector() {
        let (mut mode, _terminal, _session, _tmp) = switchable_harness().await;
        mode.handle_resume_command("").await;
        assert!(selector_mounted(&mode.ui_state), "session selector mounted");
    }

    #[tokio::test]
    async fn handle_resume_command_with_path_switches_session() {
        let (mut mode, _terminal, _session, _tmp) = switchable_harness().await;
        // A persisted session file to resume.
        let cwd = lock(&_session.session_manager()).get_cwd().to_path_buf();
        let mut manager = SessionManager::create(&cwd, None, NewSessionOptions::default())
            .expect("create persisted session");
        manager
            .append_message(user_message("resume target"))
            .expect("persist message");
        let session_file = manager
            .get_session_file()
            .expect("session file")
            .to_string_lossy()
            .into_owned();
        drop(manager);

        let old_session_id = mode.session.session_id();
        mode.handle_resume_command(&session_file).await;
        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Resumed session"), "chat: {chat}");
        assert_ne!(mode.session.session_id(), old_session_id);
    }

    #[tokio::test]
    async fn handle_resume_command_rebinds_ui_and_resubscribes() {
        let (mut mode, terminal, session, tmp) = switchable_harness().await;
        // A persisted session file to resume. Written directly:
        // `SessionManager` defers file creation to the first assistant
        // message (session-manager.ts:1015-1042), so a user-message-only
        // session never lands on disk. Written under the harness temp dir —
        // the manager's default session dir resolves CWD-relative here and
        // would litter the crate dir.
        let cwd = lock(&session.session_manager()).get_cwd().to_path_buf();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let session_path = session_dir.join("resume-target.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "resumet1",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd.to_string_lossy(),
        });
        let message = serde_json::json!({
            "type": "message",
            "id": "m-resumet1",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "resume target"}],
                "timestamp": 1_700_000_000_000i64,
            },
        });
        std::fs::write(&session_path, format!("{header}\n{message}\n")).expect("write session");
        let session_file = session_path.to_string_lossy().into_owned();

        mode.handle_resume_command(&session_file).await;

        // The ui_state session clone is swapped (the pre-fix bug left it
        // bound to the old session).
        assert_eq!(
            mode.ui_state.session().session_id(),
            mode.session.session_id()
        );
        // The chat is rebuilt from the resumed session's entries.
        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("resume target"), "chat: {chat}");

        // Events from the NEW session reach the UI (re-subscribed):
        // SessionInfoChanged → terminal title (interactive-mode.ts:2894-2898).
        mode.session.set_session_name("resumed-name");
        mode.ui_state.drain_events();
        assert!(
            terminal.title().contains("resumed-name"),
            "title: {}",
            terminal.title()
        );

        // Events from the OLD session no longer reach the UI.
        session.set_session_name("old-name");
        mode.ui_state.drain_events();
        assert!(
            !terminal.title().contains("old-name"),
            "title: {}",
            terminal.title()
        );
    }

    #[tokio::test]
    async fn handle_fork_command_forks_and_restores_editor_text() {
        let (mut mode, _terminal, session, _tmp) = switchable_harness().await;
        lock(&session.session_manager())
            .append_message(user_message("fork me"))
            .expect("append message");
        let entry_id = session
            .get_user_messages_for_forking()
            .last()
            .map(|(id, _)| id.clone())
            .expect("a forkable user message");

        let old_session_id = mode.session.session_id();
        mode.handle_fork_command(&entry_id).await;

        // Forked to a new session (interactive-mode.ts:4589-4602) and the
        // selected user message text is back in the editor.
        assert_ne!(mode.session.session_id(), old_session_id);
        assert_eq!(lock(&mode.ui_state.editor).get_text(), "fork me");
        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Forked to new session"), "chat: {chat}");
        // The ui_state session clone follows the fork.
        assert_eq!(
            mode.ui_state.session().session_id(),
            mode.session.session_id()
        );
    }

    // ---------------------------------------------------------------------
    // Bash mode
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn bash_command_streams_and_completes() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_bash_command("echo hello-from-bash-test", false);
        assert!(lock(&ui.bash_component).is_some(), "bash component mounted");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if lock(&ui.bash_component).is_none() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bash command never completed; chat: {}",
                rendered_chat(ui)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let chat = rendered_chat(ui);
        assert!(chat.contains("hello-from-bash-test"), "chat: {chat}");
        assert!(!ui.session().is_bash_running());
    }

    #[tokio::test]
    async fn flush_pending_bash_components_moves_to_chat() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        let component = Arc::new(Mutex::new(BashExecutionComponent::new(
            "echo pending",
            ui.render_handle.clone(),
            Arc::clone(&lock(&ui.theme)),
            false,
        )));
        let entry = Box::new(SharedChild(Arc::clone(&component)));
        let address = child_address(&*entry);
        lock(&ui.pending_messages_container).children.push(entry);
        lock(&ui.pending_bash_components).push((address, component));

        assert_eq!(lock(&ui.pending_messages_container).children.len(), 1);
        assert!(lock(&ui.chat_container).children.is_empty());

        ui.flush_pending_bash_components();

        assert!(lock(&ui.pending_messages_container).children.is_empty());
        assert_eq!(lock(&ui.chat_container).children.len(), 1);
        assert!(lock(&ui.pending_bash_components).is_empty());
    }

    #[test]
    fn find_exact_model_match_semantics() {
        let models = vec![
            Model {
                id: "claude-3-5-sonnet".into(),
                name: "Claude 3.5 Sonnet".into(),
                api: pir_ai::types::ApiKind("anthropic-messages".into()),
                provider: "anthropic".into(),
                base_url: String::new(),
                reasoning: false,
                thinking_level_map: None,
                input: Vec::new(),
                cost: pir_ai::types::ModelCost::default(),
                context_window: 200_000,
                max_tokens: 64_000,
                headers: None,
                compat: None,
            },
            Model {
                id: "m1".into(),
                name: "M1".into(),
                api: pir_ai::types::ApiKind("openai-completions".into()),
                provider: "custom".into(),
                base_url: String::new(),
                reasoning: false,
                thinking_level_map: None,
                input: Vec::new(),
                cost: pir_ai::types::ModelCost::default(),
                context_window: 200_000,
                max_tokens: 64_000,
                headers: None,
                compat: None,
            },
            Model {
                id: "m1".into(),
                name: "M1 alt".into(),
                api: pir_ai::types::ApiKind("openai-completions".into()),
                provider: "other".into(),
                base_url: String::new(),
                reasoning: false,
                thinking_level_map: None,
                input: Vec::new(),
                cost: pir_ai::types::ModelCost::default(),
                context_window: 200_000,
                max_tokens: 64_000,
                headers: None,
                compat: None,
            },
        ];
        // Canonical provider/id.
        assert_eq!(
            find_exact_model_reference_match("anthropic/claude-3-5-sonnet", &models)
                .map(|model| model.provider),
            Some("anthropic".to_string())
        );
        // Provider/id with different case (canonical match is case-insensitive).
        assert_eq!(
            find_exact_model_reference_match("Custom/M1", &models).map(|model| model.provider),
            Some("custom".to_string())
        );
        // Bare id that is ambiguous across providers.
        assert_eq!(find_exact_model_reference_match("m1", &models), None);
        // Empty / unknown.
        assert_eq!(find_exact_model_reference_match("  ", &models), None);
        assert_eq!(find_exact_model_reference_match("nope", &models), None);
    }

    // ---------------------------------------------------------------------
    // Clipboard paste (`handle_paste_image`, interactive-mode.ts:2629-2652)
    // ---------------------------------------------------------------------

    /// Serializes `$PATH` mutation (process-global env) across the paste
    /// tests.
    static PATH_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct PathEnvRestore {
        previous: Option<String>,
    }

    impl Drop for PathEnvRestore {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// Prepend a fake-tool dir to `$PATH` (never replace it — concurrent
    /// tests spawn `git` etc. by name).
    fn prepend_path(dir: &std::path::Path) -> PathEnvRestore {
        let previous = std::env::var("PATH").ok();
        let mut path = dir.display().to_string();
        if let Some(previous) = &previous {
            path.push(':');
            path.push_str(previous);
        }
        std::env::set_var("PATH", &path);
        PathEnvRestore { previous }
    }

    /// Write fake `xclip`/`wl-paste` executables: the `-t image/png` probe
    /// either emits PNG bytes (`image: true`) or nothing (`image: false`);
    /// plain text reads always print `pasted-text`.
    fn fake_clipboard_tools(dir: &TempDir, image: bool) -> std::path::PathBuf {
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let image_payload = if image {
            "printf '\\211PNG\\r\\n\\032\\n';\n"
        } else {
            ""
        };
        for name in ["xclip", "wl-paste"] {
            let script = bin.join(name);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = image/png ]; then\n    {image_payload}exit 0\n  fi\ndone\nprintf pasted-text\n"
                ),
            )
            .expect("write fake clipboard tool");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake clipboard tool");
            }
        }
        bin
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_with_clipboard_image_inserts_temp_path() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let bin = fake_clipboard_tools(&tmp, true);
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();

        // The PNG bytes are written to `pi-clipboard-{uuid}.png` under the
        // temp dir and the path is inserted at the cursor
        // (interactive-mode.ts:2633-2640).
        let inserted = lock(&ui.editor).get_text();
        let temp_prefix = std::env::temp_dir().display().to_string();
        assert!(
            inserted.starts_with(&temp_prefix),
            "inserted absolute temp path: {inserted}"
        );
        assert!(
            inserted.contains("pi-clipboard-") && inserted.ends_with(".png"),
            "uuid file name: {inserted}"
        );
        let bytes = std::fs::read(&inserted).expect("clipboard image file must exist");
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_file(&inserted);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_without_image_inserts_clipboard_text() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let bin = fake_clipboard_tools(&tmp, false);
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();

        assert_eq!(
            lock(&ui.editor).get_text(),
            "pasted-text",
            "text fallback inserts the clipboard text"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_without_clipboard_tools_shows_status() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The host really has a working clipboard tool (dev machine): the
        // status path is environment-dependent, so skip instead of flaking.
        // CI has none, which is what this test asserts.
        if read_clipboard_image_png().is_some() || read_clipboard_text().is_some() {
            eprintln!(
                "skipping paste_without_clipboard_tools_shows_status: host clipboard tool present"
            );
            return;
        }

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();

        assert_eq!(lock(&ui.editor).get_text(), "", "nothing to insert");
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("no clipboard tool found"),
            "status must explain the missing tool: {rendered}"
        );
    }

    // ---------------------------------------------------------------------
    // /settings on_change handlers (apply_settings_change)
    // ---------------------------------------------------------------------

    fn assistant_message(
        content: Vec<pir_ai::types::AssistantContent>,
        stop_reason: pir_ai::types::StopReason,
    ) -> AgentMessage {
        AgentMessage::Assistant(pir_ai::types::AssistantMessage {
            role: pir_ai::types::AssistantRole::Assistant,
            content,
            api: pir_ai::types::ApiKind("openai-completions".into()),
            provider: "custom".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pir_ai::types::Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 1_700_000_000_000,
        })
    }

    fn text_content(text: &str) -> pir_ai::types::AssistantContent {
        pir_ai::types::AssistantContent::Text(pir_ai::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })
    }

    fn thinking_content(text: &str) -> pir_ai::types::AssistantContent {
        pir_ai::types::AssistantContent::Thinking(pir_ai::types::ThinkingContent {
            thinking: text.to_string(),
            thinking_signature: None,
            redacted: None,
        })
    }

    fn chat_addresses(ui: &InteractiveUi) -> Vec<usize> {
        lock(&ui.chat_container)
            .children
            .iter()
            .map(|child| child_address(&**child))
            .collect()
    }

    #[tokio::test]
    async fn settings_theme_change_applies_named_theme_via_drain() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        assert_eq!(lock(&ui.theme).name.as_deref(), Some("dark"));

        apply_settings_change(ui, SettingsChange::Theme("light".to_string()));
        assert_eq!(
            session
                .settings_manager(|s| s.get_theme_setting())
                .as_deref(),
            Some("light"),
            "setting persisted"
        );
        // The theme itself is applied by the drain (ApplyThemeName), not
        // inside the selector callback.
        ui.drain_events();
        assert_eq!(lock(&ui.theme).name.as_deref(), Some("light"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn settings_theme_auto_pair_resolves_terminal_appearance() {
        // COLORFGBG drives the final detection fallback; pin it to a dark
        // background so the pair's dark branch ("light" here) is picked
        // regardless of the host environment.
        let _env_guard = crate::modes::interactive::test_support::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _restore = ColorFgBg(std::env::var_os("COLORFGBG"));
        std::env::set_var("COLORFGBG", "15;0");

        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        apply_settings_change(ui, SettingsChange::Theme("dark/light".to_string()));

        // The detection queries time out against the unstarted test Tui
        // (~300ms) before the env fallback resolves the pair.
        tokio::time::sleep(Duration::from_millis(700)).await;
        ui.drain_events();
        assert_eq!(
            lock(&ui.theme).name.as_deref(),
            Some("light"),
            "auto pair's dark-scheme branch applied"
        );
    }

    /// Restore COLORFGBG on drop (test env hygiene).
    struct ColorFgBg(Option<std::ffi::OsString>);
    impl Drop for ColorFgBg {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("COLORFGBG", value),
                None => std::env::remove_var("COLORFGBG"),
            }
        }
    }

    #[tokio::test]
    async fn settings_hide_thinking_block_change_rebuilds_chat() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(assistant_message(
                    vec![
                        thinking_content("some reasoning"),
                        text_content("the answer"),
                    ],
                    pir_ai::types::StopReason::Stop,
                ))
                .expect("append assistant");
        }
        ui.render_initial_messages();
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("some reasoning"), "visible: {rendered}");
        // Rebuild signal: a status line is a chat child but not a session
        // entry, so only a rebuild removes it. (Child addresses are
        // unreliable here — the allocator reuses them.)
        ui.show_status("rebuild-sentinel");

        apply_settings_change(ui, SettingsChange::HideThinkingBlock(true));
        assert!(*lock(&ui.hide_thinking_block));
        assert!(session.settings_manager(|s| s.get_hide_thinking_block()));

        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(!rendered.contains("rebuild-sentinel"), "chat rebuilt");
        assert!(!rendered.contains("some reasoning"), "hidden: {rendered}");
        assert!(
            rendered.contains("Thinking..."),
            "hidden label shown: {rendered}"
        );
    }

    #[tokio::test]
    async fn settings_show_cache_miss_notices_change_rebuilds_chat() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("hello"))
                .expect("append user");
        }
        ui.render_initial_messages();
        ui.show_status("rebuild-sentinel");

        apply_settings_change(ui, SettingsChange::ShowCacheMissNotices(false));
        assert!(!session.settings_manager(|s| s.get_show_cache_miss_notices()));
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(!rendered.contains("rebuild-sentinel"), "chat rebuilt");
        assert!(rendered.contains("hello"), "content kept: {rendered}");
    }

    #[tokio::test]
    async fn settings_editor_padding_x_applies_to_editor() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        let initial = lock(&ui.editor).get_padding_x();

        // The settings manager clamps editorPaddingX to <= 3
        // (settings-manager.ts:1195-1199).
        apply_settings_change(ui, SettingsChange::EditorPaddingX(3));
        assert_eq!(session.settings_manager(|s| s.get_editor_padding_x()), 3);
        assert_eq!(lock(&ui.editor).get_padding_x(), 3);
        assert_ne!(initial, 3, "test assumes a non-3 default");
    }

    #[tokio::test]
    async fn settings_autocomplete_max_visible_applies_to_editor() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;

        apply_settings_change(ui, SettingsChange::AutocompleteMaxVisible(12));
        assert_eq!(
            session.settings_manager(|s| s.get_autocomplete_max_visible()),
            12
        );
        assert_eq!(lock(&ui.editor).get_autocomplete_max_visible(), 12);
    }

    #[tokio::test]
    async fn settings_output_pad_rebuilds_chat_when_idle() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        {
            let manager = session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(user_message("hello"))
                .expect("append user");
        }
        ui.render_initial_messages();
        ui.show_status("rebuild-sentinel");

        apply_settings_change(ui, SettingsChange::OutputPad(1));
        assert_eq!(session.settings_manager(|s| s.get_output_pad()), 1);
        assert_eq!(ui.output_pad.load(Ordering::Relaxed), 1);
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            !rendered.contains("rebuild-sentinel"),
            "chat rebuilt when idle"
        );
    }

    #[tokio::test]
    async fn settings_output_pad_updates_streaming_component_in_place() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        // Start a streaming assistant message (the streaming component is
        // part of the chat tree).
        ui.push(UiCommand::MessageStart(assistant_message(
            vec![text_content("partial")],
            pir_ai::types::StopReason::Pending,
        )));
        ui.drain_events();
        assert!(lock(&ui.streaming).is_some());
        assert!(
            !chat_addresses(ui).is_empty(),
            "streaming component mounted"
        );
        // No-rebuild signal: the status sentinel survives an in-place update.
        ui.show_status("rebuild-sentinel");

        apply_settings_change(ui, SettingsChange::OutputPad(1));
        assert_eq!(ui.output_pad.load(Ordering::Relaxed), 1);
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("rebuild-sentinel"),
            "no rebuild while streaming (interactive-mode.ts:4273-4288)"
        );
        assert!(lock(&ui.streaming).is_some(), "stream survives the change");
    }

    /// Regression: selector/keybinding callbacks run on the TUI driver
    /// thread, which has no Tokio runtime — a bare `tokio::spawn` there
    /// panics with "there is no reactor running". `spawn_async` must fall
    /// back to a dedicated thread + current-thread runtime.
    #[test]
    fn spawn_async_runs_on_plain_thread_without_runtime() {
        let (tx, rx) = std::sync::mpsc::channel();
        // Simulate the driver thread: a plain OS thread with no Tokio
        // runtime context.
        let driver = std::thread::Builder::new()
            .name("pir-tui-driver-test".to_string())
            .spawn(move || {
                spawn_async(async move {
                    // Needs a real reactor: tokio::time only works inside a
                    // runtime, exactly like the async session calls.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    tx.send(42).expect("send");
                });
            })
            .expect("driver thread");
        driver.join().expect("driver joined without panic");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(42),
            "future must complete via the fallback thread runtime"
        );
    }

    /// Same callback, but invoked from inside a runtime (tests, RPC paths):
    /// `spawn_async` must use the ambient runtime and still complete. The
    /// multi-thread flavor lets the spawned task run on a worker while this
    /// test thread blocks on the channel.
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_async_uses_ambient_runtime_when_present() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_async(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx.send(7).expect("send");
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(7));
    }
}
