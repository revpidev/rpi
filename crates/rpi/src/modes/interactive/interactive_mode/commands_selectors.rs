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
//! - Login/logout selectors, the api-key login flow (T14 W6b) and the
//!   OAuth login dialog (V16-11) are wired.
//! - `handleBashCommand`: local `AgentSession::execute_bash` already records
//!   the result internally (agent-session.rs:2076), so this port does not
//!   call `record_bash_result` again (upstream interactive-mode.ts:5967).
//! - Session switches (`/new`, `/resume`, `/clone`, `/fork`, `/import`)
//!   rebind through `InteractiveMode::rebind_session_ui` (upstream
//!   `rebindCurrentSession`, interactive-mode.ts:1732-1758) instead of the
//!   runtime's `setRebindSession` hook, which stays `None`.
//! - V17-08 #63: `handleReloadCommand` mounts the reload box
//!   (`show_extension_selector`/dismiss, interactive-mode.ts:6442-6468 @
//!   v1.1.0) instead of the previous chat status line; the `/model` command
//!   probe (the second `refreshModelCatalogs` consumption point,
//!   interactive-mode.ts:5224-5249) passes a 15 s waiter token to
//!   `refresh_catalogs_shared` and warns on timeout; the built-in
//!   `show_extension_selector` / `show_extension_editor` helpers back the
//!   #60 branch-summary ask loop and the #63 `/import` dialogs.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_ai::auth::interaction::{AuthEvent, AuthInteraction, AuthPrompt};
use rpi_ai::auth::types::{AuthCheck, AuthType, BoxFutureSend, CredentialType, LoginOptions};
use rpi_ai::auth::{ModelsError, ModelsErrorCode};
use rpi_ai::types::{Model, ModelThinkingLevel, ThinkingLevel};
use rpi_tui::terminal_colors::TerminalColorScheme;
use rpi_tui::tui::{Component, Focusable, TuiStopOptions, shared_component_from_boxed};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    ClipboardPaste, EditorInput, FocusableRegion, InteractiveMode, InteractiveUi, SharedChild,
    UiCommand, child_address, install_global_keybindings, lock, remove_child_by_address,
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
use crate::core::radius::RADIUS_PROVIDER_ID;
use crate::core::session_manager::SessionManager;
use crate::core::themes::get_available_themes;
use crate::core::trust_manager::ProjectTrustStore;
use crate::extensions::llama::{LlamaHost, NotifyLevel};
use crate::modes::interactive::components::DynamicBorder;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::extension_editor::ExtensionEditorComponent;
use crate::modes::interactive::components::extension_selector::ExtensionSelectorComponent;
use crate::modes::interactive::components::llama_view::new_llama_view;
use crate::modes::interactive::components::login_dialog::LoginDialogComponent;
use crate::modes::interactive::components::model_selector::ModelSelectorComponent;
use crate::modes::interactive::components::oauth_selector::{
    AuthSelectorMode, AuthSelectorProvider, OAuthSelectorComponent,
    format_auth_selector_provider_status,
};
use crate::modes::interactive::components::scoped_models_selector::ScopedModelsSelectorComponent;
use crate::modes::interactive::components::session_selector::SessionSelectorComponent;
use crate::modes::interactive::components::settings_selector::{
    SettingsChange, SettingsSelectorComponent, SettingsSelectorOptions,
};
use crate::modes::interactive::components::thinking_selector::ThinkingSelectorComponent;
use crate::modes::interactive::components::trust_selector::{TrustOption, TrustSelectorComponent};
use crate::modes::interactive::program_status::BlockedStatus;
use crate::modes::interactive::theme_watcher::{auto_theme_pair, detect_terminal_theme_for_auto};
use crate::tools::truncate::TruncationResult;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::program_status::ProgramStatusKind;

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

/// `showExtensionSelector` (interactive-mode.ts:2690-2732) for built-in
/// callers (#60 branch-summary ask loop, #63 /import confirm): mount the
/// selector in the editor region and await the choice. `None` = Escape /
/// countdown expiry. Mirrors the extension bridge's dialog lifecycle (blocked
/// report + guarded close) without the bridge's weak UI handle.
pub(crate) async fn show_extension_selector(
    ui: &Arc<InteractiveUi>,
    title: &str,
    options: Vec<String>,
    blocked: BlockedStatus,
) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let selector = Arc::new(Mutex::new(ExtensionSelectorComponent::new(
        Arc::clone(&lock(&ui.theme)),
        Some(title.to_owned()),
        options,
        {
            let tx = tx.clone();
            Box::new(move |selected: Option<String>| {
                if let Some(tx) = lock(&tx).take() {
                    let _ = tx.send(selected);
                }
            })
        },
        {
            let tx = tx.clone();
            Box::new(move || {
                if let Some(tx) = lock(&tx).take() {
                    let _ = tx.send(None);
                }
            })
        },
        None,
    )));
    let entry = shared_component_from_boxed(Box::new(FocusableShell {
        inner: selector,
        focused: false,
    }));
    ui.show_selector(entry.clone());
    ui.set_program_status_blocked("extension-dialog", Some(blocked));
    let result = rx.await.ok().flatten();
    if ui.selector_is_current(&entry) {
        ui.hide_selector_if_current(&entry);
        ui.set_program_status_blocked("extension-dialog", None);
    }
    result
}

/// `showExtensionEditor` (interactive-mode.ts:2831-2857) for the #60 custom
/// summarization prompt: mount the multi-line editor and await submit;
/// `None` = cancel (Escape / ctrl+c).
pub(crate) async fn show_extension_editor(ui: &Arc<InteractiveUi>, title: &str) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let editor = Arc::new(Mutex::new(ExtensionEditorComponent::new(
        ui.ui.clone(),
        Arc::clone(&lock(&ui.theme)),
        title.to_owned(),
        None,
        {
            let tx = tx.clone();
            Box::new(move |text: &str| {
                if let Some(tx) = lock(&tx).take() {
                    let _ = tx.send(Some(text.to_owned()));
                }
            })
        },
        {
            let tx = tx.clone();
            Box::new(move || {
                if let Some(tx) = lock(&tx).take() {
                    let _ = tx.send(None);
                }
            })
        },
        None,
    )));
    let entry = shared_component_from_boxed(Box::new(FocusableRegion(editor)));
    ui.show_selector(entry.clone());
    ui.set_program_status_blocked(
        "extension-dialog",
        Some(BlockedStatus {
            kind: ProgramStatusKind::Question,
            message: title.to_owned(),
        }),
    );
    let result = rx.await.ok().flatten();
    if ui.selector_is_current(&entry) {
        ui.hide_selector_if_current(&entry);
        ui.set_program_status_blocked("extension-dialog", None);
    }
    result
}

/// The #60 branch-summary ask loop (interactive-mode.ts:5590-5616): `None`
/// when the user escapes (the caller re-opens the tree selector with the
/// same selection); `Some((summarize, custom_instructions))` for a complete
/// choice. A cancelled custom-prompt editor loops back to the selector.
pub(crate) async fn ask_branch_summary(ui: &Arc<InteractiveUi>) -> Option<(bool, Option<String>)> {
    loop {
        let choice = show_extension_selector(
            ui,
            "Summarize branch?",
            vec![
                "No summary".to_string(),
                "Summarize".to_string(),
                "Summarize with custom prompt".to_string(),
            ],
            BlockedStatus {
                kind: ProgramStatusKind::Question,
                message: "Summarize branch?".to_string(),
            },
        )
        .await;
        let Some(choice) = choice else {
            // Escape — re-show the tree selector with the same selection.
            return None;
        };
        if choice == "Summarize with custom prompt" {
            match show_extension_editor(ui, "Custom summarization instructions").await {
                Some(text) => return Some((true, Some(text))),
                // Cancelled — loop back to the summary selector.
                None => continue,
            }
        }
        return Some((choice != "No summary", None));
    }
}

/// `rpi_agent::types::ThinkingLevel` is a re-export of
/// `rpi_ai::types::ModelThinkingLevel` (types.rs:30-32), while the settings
/// selector uses the plain `rpi_ai::types::ThinkingLevel` (no `Off`). `Off`
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

/// `level.as_str()` equivalent for the agent-side `ThinkingLevel`
/// (`THINKING_LEVEL_OPTIONS` values, agent-session.ts:297 family).
fn agent_thinking_level_str(level: rpi_agent::types::ThinkingLevel) -> &'static str {
    match level {
        rpi_agent::types::ThinkingLevel::Off => "off",
        rpi_agent::types::ThinkingLevel::Minimal => "minimal",
        rpi_agent::types::ThinkingLevel::Low => "low",
        rpi_agent::types::ThinkingLevel::Medium => "medium",
        rpi_agent::types::ThinkingLevel::High => "high",
        rpi_agent::types::ThinkingLevel::Xhigh => "xhigh",
        rpi_agent::types::ThinkingLevel::Max => "max",
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

/// `formatHttpIdleTimeoutMs` (http-dispatcher.ts:27-38). rpi#54 / D-103
/// (V15-12 FR-B): the `0` entry renders `unlimited` (2026-09-20 user
/// ruling) instead of upstream's `disabled`; numbers persist, so this is
/// display-label only.
fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    const CHOICES: &[(u64, &str)] = &[
        (30_000, "30 sec"),
        (60_000, "1 min"),
        (120_000, "2 min"),
        (300_000, "5 min"),
        (0, "unlimited"),
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
fn build_trust_options(
    cwd: &Path,
    saved_entry: Option<&(std::path::PathBuf, bool)>,
) -> Vec<TrustOption> {
    // `isSavedOption` (trust-selector.ts:93-98 @ f2a622789): an option is
    // "saved" when its persisted path + decision match the nearest saved
    // entry; session-only variants have no saved path and never match.
    let is_saved = |saved_path: Option<&Path>, trusted: bool| match saved_entry {
        Some((entry_path, entry_decision)) => {
            saved_path.is_some_and(|path| path == entry_path) && *entry_decision == trusted
        }
        None => false,
    };
    let mut options = vec![TrustOption {
        value: "trust".to_string(),
        label: "Trust".to_string(),
        description: Some(format!("Trust {}", cwd.display())),
        saved: is_saved(Some(cwd), true),
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
            saved: is_saved(Some(parent), true),
        });
    }
    options.push(TrustOption {
        value: "trust-session".to_string(),
        label: "Trust (this session only)".to_string(),
        description: None,
        saved: false,
    });
    options.push(TrustOption {
        value: "untrust".to_string(),
        label: "Do not trust".to_string(),
        description: Some(format!("Do not trust {}", cwd.display())),
        saved: is_saved(Some(cwd), false),
    });
    options.push(TrustOption {
        value: "untrust-session".to_string(),
        label: "Do not trust (this session only)".to_string(),
        description: None,
        saved: false,
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
pub(crate) fn apply_settings_change(ui: &Arc<InteractiveUi>, change: SettingsChange) {
    let session = ui.session();
    match change {
        SettingsChange::AutoCompact(enabled) => {
            session.set_auto_compaction_enabled(enabled);
            lock(&ui.footer).set_auto_compact_enabled(enabled);
        }
        SettingsChange::ShowImages(enabled) => {
            session.settings_manager(|s| s.set_show_images(enabled));
            // `onShowImagesChange` (interactive-mode.ts:4964-4970): update
            // the mode's own field (components mounted later read it at
            // construction) and every mounted tool execution in place — no
            // rebuild, so streaming tool components keep their partial
            // output. The defaulted `Component::set_show_images` reproduces
            // the upstream `child instanceof ToolExecutionComponent` check.
            ui.show_images.store(enabled, Ordering::Relaxed);
            for child in lock(&ui.chat_container).children.iter_mut() {
                child.set_show_images(enabled);
            }
        }
        SettingsChange::ImageWidthCells(width) => {
            session.settings_manager(|s| s.set_image_width_cells(width));
            // `onImageWidthCellsChange` (interactive-mode.ts:4972-4978):
            // `setImageWidthCells` clamps to >= 1; same in-place walk as
            // the show-images arm.
            let width = width.max(1) as usize;
            ui.image_width_cells.store(width, Ordering::Relaxed);
            for child in lock(&ui.chat_container).children.iter_mut() {
                child.set_image_width_cells(width);
            }
        }
        SettingsChange::AutoResizeImages(enabled) => {
            session.settings_manager(|s| s.set_image_auto_resize(enabled));
        }
        SettingsChange::BlockImages(blocked) => {
            session.settings_manager(|s| s.set_block_images(blocked));
        }
        SettingsChange::EnableSkillCommands(enabled) => {
            session.settings_manager(|s| s.set_enable_skill_commands(enabled));
            // `onEnableSkillCommandsChange` (interactive-mode.ts:4986-4990):
            // rebuild the provider so `skill:<name>` completions appear or
            // disappear immediately.
            crate::modes::interactive::autocomplete::setup_autocomplete(ui);
        }
        SettingsChange::SteeringMode(mode) => {
            session.set_steering_mode(mode);
        }
        SettingsChange::FollowUpMode(mode) => {
            session.set_follow_up_mode(mode);
        }
        SettingsChange::Transport(transport) => {
            session.settings_manager(|s| s.set_transport(transport));
            // `onTransportChange` (interactive-mode.ts:4996-4999): persist,
            // then reassign the live agent's transport; the next request
            // reads it when assembling `StreamOptions`.
            session.agent().set_transport(transport);
        }
        SettingsChange::HttpIdleTimeoutMs(timeout_ms) => {
            session.settings_manager(|s| s.set_http_idle_timeout_ms(timeout_ms));
            // `onHttpIdleTimeoutMsChange` (interactive-mode.ts:5000-5004):
            // persist, reconfigure the live dispatcher slot (the stream
            // function reads it per request), then surface the new value.
            session.configure_http_dispatcher(timeout_ms);
            ui.show_status(&format!(
                "HTTP idle timeout: {}",
                format_http_idle_timeout_ms(timeout_ms)
            ));
        }
        SettingsChange::CacheWarmingMode(mode) => {
            // `onCacheWarmingModeChange` (interactive-mode.ts:4767-4770,
            // #9668): persist + reconcile, then surface the new mode.
            session.set_cache_warming_mode(mode);
            ui.show_status(&format!("Cache warming: {}", mode.as_str()));
        }
        SettingsChange::ModelThinkingLevelChange {
            provider,
            model_id,
            level,
        } => {
            session.settings_manager(|s| s.set_model_thinking_level(&provider, &model_id, level));
            // If the override is for the current model, apply it to the
            // session too (interactive-mode.ts:4653-4658).
            let current = session.model();
            if current
                .as_ref()
                .is_some_and(|model| model.provider == provider && model.id == model_id)
            {
                session.set_thinking_level(level);
                ui.update_editor_border_color();
            }
        }
        SettingsChange::ModelThinkingLevelRemove { provider, model_id } => {
            session.settings_manager(|s| s.remove_model_thinking_level(&provider, &model_id));
            // If the override was for the current model, revert to the
            // global default (interactive-mode.ts:4660-4668).
            let current = session.model();
            if current
                .as_ref()
                .is_some_and(|model| model.provider == provider && model.id == model_id)
            {
                let global_default = session
                    .settings_manager(|s| s.get_default_thinking_level())
                    .unwrap_or(crate::core::model_resolver::DEFAULT_THINKING_LEVEL);
                session.set_thinking_level(global_default);
                ui.update_editor_border_color();
            }
        }
        SettingsChange::Theme(theme_setting) => {
            session.settings_manager(|s| s.set_theme(&theme_setting));
            // `setThemeSetting` (theme-controller.ts:96-99): an explicit
            // in-run selection replaces the session-local override (the
            // `--use-theme` value) so later reads of
            // `currentThemeSetting ?? getThemeSetting()` prefer it.
            *lock(&ui.theme_setting_override) = Some(theme_setting.clone());
            // `applyFromSettings` (theme-controller.ts:37-60): automatic
            // pairs resolve against the terminal appearance (async); the
            // system theme regenerates from the reported colors; plain names
            // load directly. All apply through the drain (`ApplyThemeName`)
            // so `apply_theme` never runs inside this component callback
            // (lock contract). The terminal colors are queried afterwards in
            // every case (`applyFromSettings` always calls
            // `queryTerminalColors()`, theme-controller.ts:45).
            let resolved = ui.resolved_theme_name();
            let is_auto = auto_theme_pair(Some(theme_setting.as_str())).is_some()
                || resolved == crate::core::themes::SYSTEM_THEME_NAME;
            ui.ui.set_terminal_color_scheme_notifications(is_auto);
            match auto_theme_pair(Some(theme_setting.as_str())) {
                Some((light, dark)) => apply_auto_theme_pair(ui, light, dark),
                None => {
                    ui.push(UiCommand::ApplyThemeName(resolved));
                }
            }
            ui.query_terminal_colors_async();
        }
        SettingsChange::ThemePreview(_) => {
            // Ignored by the integration layer (component header).
        }
        SettingsChange::HideThinkingBlock(hidden) => {
            session.settings_manager(|s| s.set_hide_thinking_block(hidden));
            *lock(&ui.hide_thinking_block) = hidden;
            // `onHideThinkingBlockChange` → `updateThinkingBlockVisibility()`
            // (interactive-mode.ts:4668-4672 @ 9841914, b07e17faa): in-place
            // per-child update instead of the clear+rebuild — live tool
            // components keep their partial output.
            ui.update_thinking_block_visibility();
        }
        SettingsChange::MermaidRenderingMode(mode) => {
            session.settings_manager(|s| s.set_mermaid_rendering_mode(mode));
            // `chatContainer.invalidate()` (interactive-mode.ts:4483-4487):
            // drop the markdown render caches so mounted messages re-render
            // with the new mode; `requestRender()` is the common tail below.
            Component::invalidate(&mut *lock(&ui.chat_container));
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
            // `onOutputPadChange` (interactive-mode.ts:5078-5091 @ 18336987a,
            // #10557): walk the chat and pending-message containers and
            // update every child with a `setOutputPad` method in place —
            // no rebuild, so streaming tool components keep their partial
            // output. The defaulted `Component::set_output_pad` reproduces
            // the upstream `"setOutputPad" in child` duck-typing.
            for container in [&ui.chat_container, &ui.pending_messages_container] {
                for child in lock(container).children.iter_mut() {
                    child.set_output_pad(padding as usize);
                }
            }
            ui.render_handle.request_render();
        }
        SettingsChange::AutocompleteMaxVisible(max_visible) => {
            session.settings_manager(|s| s.set_autocomplete_max_visible(max_visible));
            // `setAutocompleteMaxVisible` (interactive-mode.ts:4291-4297);
            // clamped like construction.
            lock(&ui.editor).set_autocomplete_max_visible(max_visible.min(20) as usize);
        }
        SettingsChange::QuietStartup(quiet) => {
            // Tri-state `quietStartup` (V16-13 FR-D R6,
            // settings-selector.ts:924).
            session.settings_manager(|s| s.set_quiet_startup(quiet));
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
        SettingsChange::TuiMode(mode) => {
            // onTuiModeChange (interactive-mode.ts:4559-4567 @ b103937d3):
            // routed through the drain (`UiCommand::SwitchTuiMode`) — this
            // callback fires inside the settings selector's `handle_input`
            // with the renderer's inner lock held, and `switch_tui_mode`'s
            // `capture_render_state`/`stop`/`start` take that same lock
            // (non-reentrant): calling them here would self-deadlock the
            // driver thread (the "/settings tui-mode switch hang" bug).
            // The drain handler performs the switch and the
            // rollback/persist/status follow-up.
            ui.push(UiCommand::SwitchTuiMode(mode));
        }
        SettingsChange::FullscreenExitOutput(output) => {
            // onFullscreenExitOutputChange (interactive-mode.ts:4569-4571).
            session.settings_manager(|s| s.set_fullscreen_exit_output(output));
        }
        SettingsChange::FullscreenScrollbar(scrollbar_mode) => {
            // onFullscreenScrollbarChange (interactive-mode.ts:4572-4574
            // @ 6129a353b): persist + apply to the live scroll view.
            // `apply_fullscreen_scrollbar` reaches the transcript scroll
            // view directly (it survives in `fullscreen_layout_root`
            // regardless of the active renderer, so a change in regular
            // mode sticks for the next fullscreen switch); the TuiHandle
            // dispatch additionally covers the live alt-screen renderer
            // for an immediate repaint.
            session.settings_manager(|s| s.set_fullscreen_scrollbar(scrollbar_mode));
            ui.apply_fullscreen_scrollbar(scrollbar_mode);
            ui.ui.set_fullscreen_scrollbar(scrollbar_mode);
            ui.render_handle.request_render();
        }
        SettingsChange::FullscreenCopyOnSelect(enabled) => {
            // onFullscreenCopyOnSelectChange (interactive-mode.ts:4774-4776
            // @ 9841914, 4e4949299): persist + apply to the live alt-screen
            // renderer (`setCopyOnSelect`; no-op in regular mode).
            session.settings_manager(|s| s.set_fullscreen_copy_on_select(enabled));
            ui.ui.set_copy_on_select(enabled);
        }
        SettingsChange::FullscreenWheelScrollLines(lines) => {
            // onFullscreenWheelScrollLinesChange (interactive-mode.ts:4777-4779
            // @ f1927c2d5, #9758): persist + apply to the live alt-screen
            // renderer (`setWheelScrollLines`; no-op in regular mode).
            session.settings_manager(|s| s.set_fullscreen_wheel_scroll_lines(lines));
            ui.ui.set_wheel_scroll_lines(lines);
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

    async fn provider_auth(&self) -> Result<Option<rpi_ai::auth::AuthResult>, ModelsError> {
        self.ui
            .session()
            .model_runtime()
            .get_provider_auth(crate::extensions::llama::LLAMA_PROVIDER_ID, None)
            .await
    }

    async fn refresh_models(
        &self,
        options: rpi_ai::models::ModelsRefreshOptions,
    ) -> rpi_ai::models::ModelsRefreshResult {
        // The host-side `ctx.modelRegistry.refresh(options)` — the options
        // come from `llama_refresh_options` (live even in offline mode,
        // llama-only, 15s signal; index.ts:51-56).
        self.ui
            .session()
            .model_runtime()
            .refresh(Some(options))
            .await
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

/// `startProviderLogin` (interactive-mode.ts:4925-4933). `on_back` narrows
/// upstream's `onBack` callback to its only use: reopening the top-level
/// auth-type selector when a Radius login is cancelled
/// (`interactive-mode.ts:5853`).
fn start_provider_login(ui: &Arc<InteractiveUi>, provider: &AuthSelectorProvider, on_back: bool) {
    match provider.auth_type {
        AuthType::Oauth => {
            show_oauth_login_dialog(ui, provider, on_back);
        }
        AuthType::ApiKey if provider.method_login => {
            show_api_key_login_dialog(ui, provider);
        }
        AuthType::ApiKey => {
            show_ambient_auth_dialog(ui, provider);
        }
    }
}

/// `showLoginDialog` (interactive-mode.ts:6161-6210 @ ed8b3bcc1): mount the
/// login dialog, run the OAuth flow through the dialog adapter (the
/// `getDeviceId` supplier rides `LoginOptions`), complete the
/// authentication, and offer the Radius MCP setup after a Radius login.
/// Cancellation restores the editor and reopens the originating selector.
fn show_oauth_login_dialog(
    ui: &Arc<InteractiveUi>,
    provider: &AuthSelectorProvider,
    on_back: bool,
) {
    let previous_model = ui.session().model();
    let (dialog, interaction) = login_dialog_with_interaction(ui, &provider.id, &provider.name);
    mount_login_dialog(ui, &dialog);

    let ui = Arc::clone(ui);
    let provider_id = provider.id.clone();
    let provider_name = provider.name.clone();
    spawn_async(async move {
        let runtime = ui.session().model_runtime().clone();
        // `{ getDeviceId: () => this.settingsManager.getOrCreateDeviceId() }`
        // (`interactive-mode.ts:6174`): generated on first use, global scope.
        let device_ui = Arc::clone(&ui);
        let options = LoginOptions {
            get_device_id: Some(Arc::new(move || {
                device_ui
                    .session()
                    .settings_manager(|settings| settings.get_or_create_device_id())
            })),
            // Defaults to rpi's own brand name (`AGENT_NAME_HINT`).
            agent_name: None,
        };
        // `programStatus.setBlocked("login", ...)` and its `finally` clear
        // (interactive-mode.ts:6321-6335).
        ui.set_program_status_blocked(
            "login",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Auth,
                message: format!("Log in to {provider_name}"),
            }),
        );
        let result = runtime
            .login(&provider_id, AuthType::Oauth, &interaction, Some(&options))
            .await;
        ui.set_program_status_blocked("login", None);
        ui.hide_selector();
        match result {
            Ok(_credential) => {
                if let Err(error) = complete_provider_authentication(
                    &ui,
                    &provider_id,
                    &provider_name,
                    AuthType::Oauth,
                    previous_model,
                )
                .await
                {
                    let message = error.message;
                    if message.starts_with("Credential login committed for") {
                        // `CredentialSynchronizationError` (interactive-mode.ts:6200-6202).
                        ui.show_error(&format!(
                            "Logged in to {provider_name}, but local model state could not be synchronized: {message}"
                        ));
                    } else if message != "Login cancelled" {
                        ui.show_error(&format!("Failed to login to {provider_name}: {message}"));
                    }
                } else if provider_id == RADIUS_PROVIDER_ID {
                    offer_radius_mcp_server(&ui, &provider_id, &provider_name);
                }
            }
            Err(error) => {
                if error.message == "Login cancelled" {
                    if on_back {
                        show_login_auth_type_selector(&ui, None);
                    }
                } else {
                    ui.show_error(&format!(
                        "Failed to login to {provider_name}: {}",
                        error.message
                    ));
                }
            }
        }
    });
}

/// `offerRadiusMcpServer` (interactive-mode.ts:6296-6344 @ ed8b3bcc1): offer
/// to point the Radius MCP server in the global `mcp.json` at the Radius
/// login, adding the server when missing. Nothing is asked when a global
/// server already uses this login.
fn offer_radius_mcp_server(ui: &Arc<InteractiveUi>, provider_id: &str, provider_name: &str) {
    use crate::extensions::mcp::config::{
        McpAuthConfig, McpHttpServerConfig, McpServerCommon, McpServerConfig,
        add_mcp_server_config, load_mcp_config,
    };

    let agent_dir = crate::config::get_agent_dir();
    let mcp_path = agent_dir.join("mcp.json");
    let mcp_url = crate::core::radius::radius_mcp_url();
    let normalize_url = |url: &str| url.trim_end_matches('/').to_owned();
    let loaded = load_mcp_config(&agent_dir, Path::new(ui.session().cwd()), false);
    let servers = loaded.servers;
    let existing = servers.iter().find(|entry| match &entry.config {
        McpServerConfig::Http(http) => normalize_url(&http.url) == normalize_url(&mcp_url),
        McpServerConfig::Stdio(_) => false,
    });
    if let Some(McpServerConfig::Http(http)) = existing.map(|entry| &entry.config)
        && http
            .auth
            .as_ref()
            .is_some_and(|auth_config| auth_config.provider == provider_id)
    {
        return;
    }

    let mut name = existing
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| "radius".to_owned());
    if existing.is_none() && servers.iter().any(|entry| entry.name == name) {
        name = "radius-mcp".to_owned();
    }
    let mut config = match existing {
        Some(entry) => entry.config.clone(),
        None => McpServerConfig::Http(McpHttpServerConfig {
            common: McpServerCommon::default(),
            kind: None,
            url: mcp_url,
            headers: None,
            oauth: None,
            auth: None,
            extra: serde_json::Map::new(),
        }),
    };
    if let McpServerConfig::Http(http) = &mut config {
        http.auth = Some(McpAuthConfig {
            provider: provider_id.to_owned(),
        });
        // `auth` replaces the MCP OAuth sign-in.
        http.oauth = None;
    }

    let path_display = mcp_path.display().to_string();
    let select_ui = Arc::clone(ui);
    let selector = Arc::new(Mutex::new(ExtensionSelectorComponent::new(
        Arc::clone(&lock(&ui.theme)),
        Some(format!("Configure {provider_name} MCP in {path_display}?")),
        vec!["Yes".to_owned(), "No".to_owned()],
        Box::new(move |option: Option<String>| {
            select_ui.hide_selector();
            if option.as_deref() != Some("Yes") {
                return;
            }
            match add_mcp_server_config(&mcp_path, &name, &config) {
                Ok(_) => select_ui.handle_reload_command(),
                Err(error) => {
                    select_ui.show_error(&format!("Could not update {path_display}: {error}"));
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
        None,
    )));
    let entry = shared_component_from_boxed(Box::new(FocusableShell {
        inner: selector,
        focused: false,
    }));
    ui.show_selector(entry);
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
        // Async `show_*` updates from the spawned login task must schedule a
        // re-render (upstream calls `this.tui.requestRender()` in each show
        // method; login-dialog.ts:112, 130, …).
        let render_handle = ui.render_handle.clone();
        dialog_guard.on_state_change = Some(Box::new(move || {
            render_handle.request_render();
        }));
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
        // `programStatus.setBlocked("login", ...)` and its `finally` clear
        // (interactive-mode.ts:6321-6335).
        ui.set_program_status_blocked(
            "login",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Auth,
                message: format!("Log in to {provider_name}"),
            }),
        );
        let result = runtime
            .login(&provider_id, AuthType::ApiKey, &interaction, None)
            .await;
        ui.set_program_status_blocked("login", None);
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
                    && error.message != "Login cancelled"
                {
                    ui.show_error(&format!(
                        "Failed to save API key for {provider_name}: {}",
                        error.message
                    ));
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
        let render_handle = ui.render_handle.clone();
        dialog_guard.on_state_change = Some(Box::new(move || {
            render_handle.request_render();
        }));
        let ui_restore = Arc::clone(ui);
        dialog_guard.on_cancel = Some(Box::new(move || {
            // `onComplete` → `restoreEditor`
            // (interactive-mode.ts:5146-5147, 5184-5189).
            ui_restore.hide_selector();
        }));
        dialog_guard.show_info(
            &format!(
                "{} is configured outside rpi.",
                provider.method_name.as_deref().unwrap_or("Authentication")
            ),
            vec![],
            true,
        );
    }
    mount_login_dialog(ui, &dialog);
}

/// `llamaCppPostLoginGuidance` (interactive-mode.ts:290-294 @ a1bc0ec79,
/// #8203): llama.cpp has no default model — direct users to `/llama` first
/// when nothing is loaded. The provider id matches
/// `LLAMA_PROVIDER_ID` but stays inline (upstream keeps it decoupled from
/// the built-in extension).
fn llama_cpp_post_login_guidance(action_label: &str, loaded_model_count: usize) -> String {
    if loaded_model_count == 0 {
        format!(
            "{action_label}. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
        )
    } else {
        format!(
            "{action_label}. Use /model to select a loaded llama.cpp model, or /llama to manage models."
        )
    }
}

/// `finishAuthentication` (interactive-mode.ts:5691-5726 @ 9767ba275):
/// the immediate selection pass — default-model auto-selection when the
/// agent still runs the "unknown" placeholder, the footer/editor refresh,
/// and the status line. Extracted from `completeProviderAuthentication` so
/// the deferred path can re-run it after catalog discovery completes.
async fn finish_authentication(
    ui: &Arc<InteractiveUi>,
    runtime: &ModelRuntime,
    provider_id: &str,
    action_label: &str,
    previous_model: Option<Model>,
) -> Result<(), ModelsError> {
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
        // llama.cpp has no default model: dedicated post-login guidance
        // (a1bc0ec79, #8203) — `--no-models-autoload` servers only store
        // the connection; /llama loads a model first.
        if provider_id == "llama.cpp" {
            selection_error = Some(llama_cpp_post_login_guidance(
                action_label,
                provider_models.len(),
            ));
        } else {
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
                        .cloned()
                        // Radius catalogs vary by account; prefer
                        // `balanced`, then use catalog order
                        // (9767ba275).
                        .or_else(|| {
                            (provider_id == "radius")
                                .then(|| provider_models.first().cloned())
                                .flatten()
                        });
                    match candidate {
                        None => {
                            selection_error = Some(format!(
                                "{action_label}, but its default model \"{default_model_id}\" is not available. Use /model to select a model."
                            ));
                        }
                        // `setModel(selectedModel, { persist: true })`
                        // (interactive-mode.ts:5698 @ 2ff8ba622): the post-login
                        // default selection persists (V14-13 catch-up — the
                        // plain call silently became session-only in V14-12).
                        Some(model) => {
                            match ui
                                .session()
                                .set_model_with_options(
                                    model.clone(),
                                    crate::core::agent_session::ModelMutationOptions {
                                        persist: true,
                                    },
                                )
                                .await
                            {
                                Ok(()) => selected_model = Some(model),
                                Err(error) => {
                                    selected_model = None;
                                    selection_error = Some(format!(
                                        "{action_label}, but selecting its default model failed: {error}. Use /model to select a model."
                                    ));
                                }
                            }
                        }
                    }
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
        // `void this.maybeWarnAboutAnthropicSubscriptionAuth(selectedModel)`
        // (interactive-mode.ts:6120-6126 @ abe508e1b). The login flow no
        // longer carries the removed upstream `checkDaxnutsEasterEgg`
        // (ADR-0036 decision 5; upstream deleted it in v1.0.1).
        ui.maybe_warn_about_anthropic_subscription_auth(Some(selected.clone()))
            .await;
    } else {
        ui.show_status(&format!(
            "{action_label}. Credentials saved to {}",
            auth_path.display()
        ));
        if let Some(selection_error) = selection_error {
            ui.show_error(&selection_error);
        } else {
            ui.maybe_warn_about_anthropic_subscription_auth(None).await;
        }
    }
    Ok(())
}

/// `completeProviderAuthentication` (interactive-mode.ts:5675-5737 @
/// 9767ba275): post-login default-model selection, waiting for catalog
/// discovery when the default model is not in the available snapshot yet
/// ("select Radius models after catalog discovery"). Dynamic catalogs may
/// be empty until the first authenticated network refresh, so the
/// selection defers to after the scoped refresh completes instead of
/// erroring with "default model not available"; a session/model change
/// made while the refresh ran cancels the deferred pick.
/// `deferSelection` (interactive-mode.ts:5681-5685 @ 9767ba275): dynamic
/// catalogs may be empty until the first authenticated network refresh, so
/// when the agent still runs the "unknown" placeholder, a default model is
/// configured for the provider, and that default is not in the available
/// snapshot yet, the post-login selection waits for catalog discovery
/// instead of erroring with "default model not available".
fn post_login_selection_should_defer(
    previous_model: Option<&Model>,
    provider_id: &str,
    available: &[Model],
) -> bool {
    previous_model.is_none()
        && default_model_for_provider(provider_id).is_some()
        && !available.iter().any(|model| {
            model.provider == provider_id
                && Some(model.id.as_str()) == default_model_for_provider(provider_id)
        })
}

async fn complete_provider_authentication(
    ui: &Arc<InteractiveUi>,
    provider_id: &str,
    provider_name: &str,
    auth_type: AuthType,
    previous_model: Option<Model>,
) -> Result<(), ModelsError> {
    let runtime = ui.session().model_runtime().clone();
    // Upstream guards the deferred re-selection with BOTH the session
    // identity and the model (`this.session === session && session.model ===
    // previousModel`, interactive-mode.ts:5889-5892); a session switch during
    // the 15 s refresh window must not select a model on the new session.
    let session_id = ui.session().session_id();
    let _ = runtime.get_available(None).await?;
    let action_label = match auth_type {
        AuthType::Oauth => format!("Logged in to {provider_name}"),
        AuthType::ApiKey => format!("Saved API key for {provider_name}"),
    };
    let defer_selection = post_login_selection_should_defer(
        previous_model.as_ref(),
        provider_id,
        &runtime.get_available(None).await?,
    );

    // `getAuthPath()` (config.ts:534-536): `{agentDir}/auth.json`.
    let auth_path = lock(&ui.session().resource_loader())
        .agent_dir()
        .join("auth.json");
    if defer_selection {
        ui.show_status(&format!(
            "{action_label}. Credentials saved to {}. Refreshing model catalog…",
            auth_path.display()
        ));
    } else {
        finish_authentication(
            ui,
            &runtime,
            provider_id,
            &action_label,
            previous_model.clone(),
        )
        .await?;
    }

    // Scoped network refresh with the hard 15s abort (interactive-mode.ts:
    // 5713-5714) — fire-and-forget upstream (`void … .then(…)`), so the
    // completion path runs detached here too.
    let refresh_ui = Arc::clone(ui);
    let refresh_runtime = runtime.clone();
    let refresh_provider_id = provider_id.to_owned();
    let refresh_action_label = action_label.clone();
    let refresh_previous_model = previous_model.clone();
    let refresh_session_id = session_id.clone();
    tokio::spawn(async move {
        let token = CancellationToken::new();
        let abort = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15_000)).await;
            abort.cancel();
        });
        let result = refresh_runtime
            .refresh(Some(rpi_ai::models::ModelsRefreshOptions {
                providers: Some(vec![refresh_provider_id.clone()]),
                signal: Some(token),
                ..Default::default()
            }))
            .await;
        if result.aborted {
            refresh_ui.show_warning(&format!(
                "{refresh_action_label}, but its model catalog refresh timed out; using cached models."
            ));
        } else if !result.errors.is_empty() {
            refresh_ui.show_warning(&format!(
                "{refresh_action_label}, but its model catalog could not be refreshed; using cached models."
            ));
        }
        // Do not replace a model or session selected while the refresh was
        // running (interactive-mode.ts:5720-5722, 5889-5892).
        if defer_selection
            && refresh_ui.session().session_id() == refresh_session_id
            && refresh_ui.session().model() == refresh_previous_model
        {
            let _ = finish_authentication(
                &refresh_ui,
                &refresh_runtime,
                &refresh_provider_id,
                &refresh_action_label,
                refresh_previous_model,
            )
            .await;
        }
        refresh_ui.update_available_provider_count();
        Component::invalidate(&mut *lock(&refresh_ui.footer));
        refresh_ui.render_handle.request_render();
    });
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
    // rpi collapses `method.loginLabel` (helpers.ts `lazyOAuth`) for every
    // provider — the uniform "Sign in with an account" row (upstream
    // swaps in the provider's own label, e.g. meta's "Sign in with Meta",
    // oauthLoginLabel interactive-mode.ts:5648-5650 @ 19451accd; the
    // kimi/xai/openrouter collapse precedent predates meta).
    let subscription_label = "Sign in with an account".to_owned();
    let api_key_label = "Sign in with an API key".to_owned();
    // The top-level selector offers Radius directly, as its last option
    // (`ed8b3bcc1`); a per-provider method select never adds it. The row
    // carries the cached auth status indicator.
    let radius_option = if provider_options.is_none() {
        ui.session()
            .model_runtime()
            .get_providers()
            .iter()
            .find(|provider| provider.id() == RADIUS_PROVIDER_ID)
            .and_then(|provider| {
                AuthSelectorProvider::from_provider(
                    provider,
                    ui.session()
                        .model_runtime()
                        .get_provider_auth_status(provider.id()),
                )
                .into_iter()
                .find(|option| option.auth_type == AuthType::Oauth)
            })
    } else {
        None
    };
    let radius_label = radius_option.as_ref().map(|option| {
        format!(
            "Sign in with {}{}",
            option.name,
            format_auth_selector_provider_status(&lock(&ui.theme), option)
        )
    });
    let has_oauth = provider_options
        .as_ref()
        .map(|providers| providers.iter().any(|p| p.auth_type == AuthType::Oauth))
        .unwrap_or(true);
    let has_api_key = provider_options
        .as_ref()
        .map(|providers| providers.iter().any(|p| p.auth_type == AuthType::ApiKey))
        .unwrap_or(true);
    let mut options: Vec<String> = Vec::new();
    if has_oauth {
        options.push(subscription_label.clone());
    }
    if has_api_key {
        options.push(api_key_label.clone());
    }
    if let Some(label) = &radius_label {
        options.push(label.clone());
    }
    if options.is_empty() {
        ui.show_status("No login methods available.");
        return;
    }
    if let Some(providers) = &provider_options
        && options.len() == 1
    {
        // Only one method: start it directly (interactive-mode.ts:4957-
        // 4962).
        if let Some(provider) = providers.first() {
            start_provider_login(ui, provider, false);
        }
        return;
    }

    let title = match &provider_options {
        Some(providers) => match providers.first() {
            Some(provider) => format!("Select authentication method for {}:", provider.name),
            None => "Select authentication method:".to_owned(),
        },
        None => "Select authentication method:".to_owned(),
    };
    let select_ui = Arc::clone(ui);
    let selector = Arc::new(Mutex::new(ExtensionSelectorComponent::new(
        Arc::clone(&lock(&ui.theme)),
        Some(title),
        options,
        Box::new(move |option: Option<String>| {
            select_ui.hide_selector();
            let Some(option) = option else {
                return;
            };
            if radius_label.as_deref() == Some(option.as_str()) {
                if let Some(radius) = &radius_option {
                    start_provider_login(&select_ui, radius, true);
                }
                return;
            }
            let auth_type = if option == subscription_label {
                AuthType::Oauth
            } else {
                AuthType::ApiKey
            };
            match &provider_options {
                Some(providers) => {
                    if let Some(provider) = providers.iter().find(|p| p.auth_type == auth_type) {
                        start_provider_login(&select_ui, provider, false);
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
            cache_warming_mode: session.settings_manager(|s| s.get_cache_warming_mode()),
            // `thinkingLevel` here is the global DEFAULT (not the session
            // level) — interactive-mode.ts:4577.
            thinking_level: model_thinking_to_setting(
                session
                    .settings_manager(|s| s.get_default_thinking_level())
                    .unwrap_or(crate::core::model_resolver::DEFAULT_THINKING_LEVEL),
            ),
            model_thinking_levels: session.settings_manager(|s| s.get_all_model_thinking_levels()),
            available_default_models: session.model_runtime().get_available_snapshot(),
            current_model: session.model(),
            default_model: session.settings_manager(|s| {
                match (s.get_default_provider(), s.get_default_model()) {
                    (Some(provider), Some(id)) => format!("{provider}/{id}"),
                    _ => "not set".to_string(),
                }
            }),
            current_theme: session
                .settings_manager(|s| s.get_theme_setting())
                .unwrap_or_else(|| crate::core::themes::SYSTEM_THEME_NAME.to_string()),
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
            mermaid_rendering_mode: session.settings_manager(|s| s.get_mermaid_rendering_mode()),
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
            tui_mode: ui.ui.mode(),
            fullscreen_exit_output: session.settings_manager(|s| s.get_fullscreen_exit_output()),
            fullscreen_scrollbar: session.settings_manager(|s| s.get_fullscreen_scrollbar()),
            fullscreen_copy_on_select: session
                .settings_manager(|s| s.get_fullscreen_copy_on_select()),
            fullscreen_wheel_scroll_lines: session
                .settings_manager(|s| s.get_fullscreen_wheel_scroll_lines()),
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

        // Store a weak ref for the TuiMode rollback path
        // (interactive-mode.ts:4561 `selector.getSettingsList().updateValue(
        // "tui-mode", this.ui.mode)`).
        *lock(&ui.settings_selector_weak) = Some(Arc::downgrade(&selector));

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
            let ui = Arc::clone(ui);
            Box::new(move |model: &Model| {
                let session = session.clone();
                let ui = Arc::clone(&ui);
                let model = model.clone();
                spawn_async(async move {
                    // `selectModel(model, persist = true)`
                    // (interactive-mode.ts:4992-5009 @ 2ff8ba622): the
                    // Ctrl+S path switches the model AND persists the
                    // global default (+ scope), then reports
                    // `Default model: provider/id`.
                    match session
                        .set_model_with_options(
                            model.clone(),
                            crate::core::agent_session::ModelMutationOptions { persist: true },
                        )
                        .await
                    {
                        Ok(()) => {
                            ui.hide_selector();
                            ui.update_editor_border_color();
                            ui.show_status(&format!(
                                "Default model: {}/{}",
                                model.provider, model.id
                            ));
                            ui.maybe_warn_about_anthropic_subscription_auth(Some(model))
                                .await;
                        }
                        Err(error) => {
                            ui.hide_selector();
                            ui.show_error(&error.raw_message());
                        }
                    }
                });
            })
        };
        // `defaultProvider && defaultModel ? { provider, id } : undefined`
        // (interactive-mode.ts:5024, 1d3503fb9) — the ` · default` badge
        // and "default"-search source.
        let default_model: Option<(String, String)> =
            session.settings_manager(
                |s| match (s.get_default_provider(), s.get_default_model()) {
                    (Some(provider), Some(id)) => Some((provider, id)),
                    _ => None,
                },
            );
        let on_select = {
            let session = session.clone();
            let ui = Arc::clone(ui);
            Box::new(move |model: Model| {
                let session = session.clone();
                let ui = Arc::clone(&ui);
                spawn_async(async move {
                    // `onSelect` → `selectModel(model, false)` —
                    // session-scoped selection, zero settings writes
                    // (2ff8ba622, #8356).
                    match session.set_model(model.clone()).await {
                        Ok(()) => {
                            ui.hide_selector();
                            ui.update_editor_border_color();
                            ui.show_status(&format!("Model: {}", model.id));
                            ui.maybe_warn_about_anthropic_subscription_auth(Some(model))
                                .await;
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
            default_model,
        )));
        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `showThinkingSelector` (interactive-mode.ts:4819-4839 @ 2ff8ba622):
    /// session-only select + Ctrl+S persist-as-default, with the persisted
    /// default shown as `· default`.
    pub(crate) fn show_thinking_selector(ui: &Arc<Self>) {
        let session = ui.session();
        // `this.session.thinkingLevel ?? DEFAULT_THINKING_LEVEL`
        // (interactive-mode.ts:4829).
        let current_level = session.thinking_level();
        let available_levels = session.get_available_thinking_levels();
        let default_level = session
            .settings_manager(|s| s.get_default_thinking_level())
            .unwrap_or(crate::core::model_resolver::DEFAULT_THINKING_LEVEL);

        // `selectLevel(level, false)` (interactive-mode.ts:4809-4818):
        // session-scoped, status `Thinking level: {level}`.
        let on_select = {
            let ui = Arc::clone(ui);
            Box::new(move |level: rpi_agent::types::ThinkingLevel| {
                ui.session().set_thinking_level(level);
                ui.update_editor_border_color();
                ui.hide_selector();
                ui.show_status(&format!(
                    "Thinking level: {}",
                    agent_thinking_level_str(level)
                ));
            })
        };
        let on_cancel = {
            let ui = Arc::clone(ui);
            Box::new(move || {
                ui.hide_selector();
                ui.render_handle.request_render();
            })
        };
        // `selectLevel(level, true)`: Ctrl+S persists the default,
        // status `Default thinking level: {level}`.
        let on_select_as_default = {
            let ui = Arc::clone(ui);
            Box::new(move |level: rpi_agent::types::ThinkingLevel| {
                ui.session().set_thinking_level_with_options(
                    level,
                    crate::core::agent_session::ModelMutationOptions { persist: true },
                );
                ui.update_editor_border_color();
                ui.hide_selector();
                ui.show_status(&format!(
                    "Default thinking level: {}",
                    agent_thinking_level_str(level)
                ));
            })
        };

        let selector = Arc::new(Mutex::new(ThinkingSelectorComponent::new(
            Arc::clone(&lock(&ui.theme)),
            current_level,
            available_levels,
            on_select,
            on_cancel,
            Some(on_select_as_default),
            Some(default_level),
        )));
        let entry = shared_component_from_boxed(Box::new(FocusableRegion(selector)));
        ui.show_selector(entry);
    }

    /// `handleThinkingCommand` (interactive-mode.ts:4791-4806 @ 496185f6e):
    /// no argument opens the selector; an exact level name applies
    /// session-only; anything else is an error.
    pub(crate) fn handle_thinking_command(&self, search_term: &str) {
        let search_term = search_term.trim();
        let Some(ui) = self.upgrade_self() else {
            return;
        };
        if search_term.is_empty() {
            Self::show_thinking_selector(&ui);
            return;
        }

        let normalized = search_term.to_lowercase();
        let available = self.session().get_available_thinking_levels();
        let level = available
            .iter()
            .find(|candidate| agent_thinking_level_str(**candidate) == normalized);
        match level {
            Some(level) => {
                // `selectThinkingLevel(level, false)` — session-scoped.
                self.session().set_thinking_level(*level);
                self.update_editor_border_color();
                self.show_status(&format!(
                    "Thinking level: {}",
                    agent_thinking_level_str(*level)
                ));
            }
            None => {
                let available_names: Vec<&str> = available
                    .iter()
                    .map(|level| agent_thinking_level_str(*level))
                    .collect();
                self.show_error(&format!(
                    "Unknown thinking level \"{search_term}\". Available levels: {}.",
                    available_names.join(", ")
                ));
            }
        }
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
        let load_wake_ui = Arc::clone(ui);
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
            // V17-08 FR-C (#64): deletion lives inside the component
            // (`deleteSessionFile`, session-selector.ts:654-688); `on_delete`
            // is a post-deletion observer only. Upstream's interactive mount
            // passes no observer, so the component-internal chain (trash →
            // unlink + method-dependent header status) runs alone.
            None,
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
            // dfbf793b7: loader threads wake the run loop's UiCommand drain
            // after every publish (the component itself also requests a
            // render through its `TuiHandle`).
            Some(Arc::new(move || {
                load_wake_ui.push(UiCommand::SessionListEvent);
            })),
        )));
        let entry = shared_component_from_boxed(Box::new(FocusableRegion(Arc::clone(&selector))));
        ui.show_selector(entry);
        // Typed drain target for `UiCommand::SessionListEvent` (the entry
        // above is type-erased as a `SharedComponent`). Registered AFTER
        // `show_selector` — it clears the slot when superseding a previous
        // selector.
        *lock(&ui.session_selector) = Some(selector);
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
        // The nearest saved decision (findNearestTrustEntry) feeds the
        // per-option saved markers.
        let saved_entry = store.get_entry(&cwd).ok().flatten();
        let options = build_trust_options(&cwd, saved_entry.as_ref());

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
                        "Saved trust decision: {}. Restart rpi for this to take effect.",
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

    /// `showLoginProviderSelector(authType?, initialSearchInput?)`
    /// (interactive-mode.ts:4993-5034): the login provider list, optionally
    /// filtered to one auth method (the bare `/login` pre-selector path).
    /// Selecting a row dispatches through `startProviderLogin` — the api-key
    /// dialog, the ambient-info dialog, or the OAuth login dialog (V16-11).
    /// `initial_search_input` mirrors the upstream `initialSearchInput`
    /// (pre-filled fuzzy search, e.g. the unmatched `/login <ref>` fallback).
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
                start_provider_login(&select_ui, &provider, false);
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
                    subscription: None,
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
                        Err(error) => ui.show_error(&format!("Logout failed: {error}")),
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
            start_provider_login(&ui, &matches[0], false);
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
            // Second `refreshModelCatalogs` consumption point
            // (interactive-mode.ts:5224-5249): a 15 s controller bounds the
            // exact-match probe; a timeout warns and searches the cached
            // models. Shared in-flight refresh (7d8c11d37): /model joins a
            // running catalog refresh instead of restarting it.
            {
                let cancel = CancellationToken::new();
                let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let timeout_task = {
                    let cancel = cancel.clone();
                    let timed_out = Arc::clone(&timed_out);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(15)).await;
                        timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                        cancel.cancel();
                    })
                };
                let result = self
                    .session()
                    .model_runtime()
                    .refresh_catalogs_shared(Some(cancel))
                    .await;
                timeout_task.abort();
                if result.aborted && timed_out.load(std::sync::atomic::Ordering::SeqCst) {
                    self.show_warning("Model refresh timed out; searching cached models.");
                }
            };
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
                    self.maybe_warn_about_anthropic_subscription_auth(Some(model))
                        .await;
                }
                Err(error) => self.show_error(&error.raw_message()),
            },
            None => Self::show_model_selector(&ui, Some(search_term.to_string())),
        }
    }

    /// `handleReloadCommand` (interactive-mode.ts:6430-6518): mount the
    /// reload box (`Reloading keybindings, extensions, skills, prompts,
    /// themes, and context files...`) in the editor region, reload the
    /// session in the background, then dismiss the box and restore the
    /// editor.
    pub(crate) fn handle_reload_command(&self) {
        if self.session().is_streaming() {
            self.show_warning("Wait for the current response to finish before reloading.");
            return;
        }
        if self.session().is_compacting() {
            self.show_warning("Wait for compaction to finish before reloading.");
            return;
        }
        // `resetExtensionUI()` (interactive-mode.ts:6440): the reload
        // re-runs extension factories, which re-register their declarative
        // autocomplete sources (V17-11 FR-B R4).
        self.reset_extension_autocomplete();
        let Some(ui) = self.upgrade_self() else {
            return;
        };

        // Reload box (interactive-mode.ts:6442-6462): bordered container
        // with the muted reloading notice, swapped into the editor region
        // and focused until the reload settles.
        let reload_box = {
            let border_color = {
                let theme = Arc::clone(&lock(&ui.theme));
                Box::new(move |text: &str| theme.fg("border", text))
            };
            let mut container = rpi_tui::tui::Container::new();
            container.add_child(Box::new(DynamicBorder::new(border_color.clone())));
            container.add_child(Box::new(Spacer::new(1)));
            container.add_child(Box::new(Text::new(
                lock(&ui.theme).fg(
                    "muted",
                    "Reloading keybindings, extensions, skills, prompts, themes, and context files...",
                ),
                1,
                0,
                None,
            )));
            container.add_child(Box::new(Spacer::new(1)));
            container.add_child(Box::new(DynamicBorder::new(border_color)));
            shared_component_from_boxed(Box::new(container))
        };
        ui.show_selector(reload_box.clone());
        ui.render_handle.request_render();

        // `session.reload()` (agent-session.ts:2600-2628) — the upstream
        // reload-box body (:6470): settings/resources reload + extension
        // host reload (factories re-run, factory cache generation bumped) +
        // tool-registry rebuild + session_start/resources_discover(reload)
        // events. Runs in the background so the TUI stays responsive.
        let session = self.session();
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
                // `dismissReloadBox` (interactive-mode.ts:6463-6468):
                // restore the editor unless another selector superseded the
                // box meanwhile.
                ui.hide_selector_if_current(&reload_box);
                ui.render_handle.request_render();
            }
        });
        // Re-install both keybinding globals (re-reads the config file,
        // keybindings.ts:354-357).
        install_global_keybindings();
        // Extension-command conflicts with built-ins
        // (interactive-mode.ts:710-723; upstream folds the diagnostics into
        // the loaded-resources block at :1944). The registered-command list
        // comes from the live runner.
        let commands = self.session().extension_runner().registered_commands();
        for diagnostic in
            crate::modes::interactive::autocomplete::get_builtin_command_conflict_diagnostics(
                &commands,
            )
        {
            // Upstream renders the path line above the message
            // (`formatDiagnostics`); the status-line consumer joins them.
            let message = match &diagnostic.path {
                Some(path) => format!("{} ({})", diagnostic.message, path.display()),
                None => diagnostic.message.clone(),
            };
            self.show_warning(&message);
        }
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
            self.output_pad.load(Ordering::Relaxed),
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
            // `user_bash` extension interception (interactive-mode.ts:6509-6524):
            // a full `result` replacement skips local execution entirely;
            // handler errors or invalid defined results **fail closed**
            // (#9068, `509ee2bd0`) — no later handlers, no local execution
            // (the runner already reported the error; upstream returns
            // silently from the catch arm). Only `Ok(None)` falls through.
            let runner = session.extension_runner();
            // Owned so a declarative `rewrite` rule can replace it before
            // local execution (V17-11 FR-B R5).
            let mut command = command.clone();
            if runner.has_handlers("user_bash") {
                let cwd = session.cwd().to_owned();
                match runner
                    .emit_user_bash(&command, exclude_from_context, &cwd)
                    .await
                {
                    Ok(crate::core::extensions::UserBashDecision::Replace(result)) => {
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
                    // Declarative policy denied the command: no local
                    // execution, the rule's message is the failure output
                    // (V17-11 FR-B R5).
                    Ok(crate::core::extensions::UserBashDecision::Deny(message)) => {
                        lock(&component).append_output(&message);
                        lock(&component).set_complete(Some(1), false, None, None);
                        let result = crate::tools::bash_executor::BashResult {
                            output: message,
                            exit_code: Some(1),
                            cancelled: false,
                            truncated: false,
                            full_output_path: None,
                        };
                        session.record_bash_result(&command, &result, exclude_from_context);
                        if let Some(ui) = &ui {
                            *lock(&ui.bash_component) = None;
                            ui.render_handle.request_render();
                        }
                        return;
                    }
                    Ok(crate::core::extensions::UserBashDecision::Rewrite(rewritten)) => {
                        command = rewritten;
                    }
                    Ok(crate::core::extensions::UserBashDecision::RunLocal) => {}
                    Err(_) => {
                        // Fail-closed: complete the mounted component with no
                        // result and stop — never execute locally. The error
                        // already surfaced through the runner's onError
                        // listeners (extension error surface).
                        lock(&component).set_complete(None, false, None, None);
                        if let Some(ui) = &ui {
                            *lock(&ui.bash_component) = None;
                            ui.render_handle.request_render();
                        }
                        return;
                    }
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
            // interactive-mode.ts:5554-5555: clone failure is a recoverable
            // `showError`, unlike the five fatal call sites (create ×2
            // :1986/:6937, fork :1998, resume :5784, import :6607).
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
            // interactive-mode.ts:1844 @ 4181f66: fork failure is fatal.
            Err(error) => {
                self.handle_fatal_runtime_error("Failed to fork session", &error.raw_message())
                    .await
            }
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
            // interactive-mode.ts:6222 @ 4181f66: create failure is fatal.
            Err(error) => {
                self.handle_fatal_runtime_error("Failed to create session", &error.raw_message())
                    .await
            }
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
                        use rpi_ext_host::api::UiBridge;
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
            // interactive-mode.ts:5164 @ 4181f66: resume failure is fatal.
            Err(error) => {
                self.handle_fatal_runtime_error("Failed to resume session", &error.raw_message())
                    .await
            }
        }
    }

    /// `rebindCurrentSession({ renderBeforeBind: true })`
    /// (interactive-mode.ts:1732-1758): detach the old subscription, swap the
    /// bound session everywhere, rebuild the chat from the new session's
    /// entries and re-subscribe. Called by every session-switching command
    /// (`/new`, `/resume`, `/clone`, `/fork`, `/import`); the runtime's
    /// `setRebindSession` hook stays `None` (module header).
    pub(crate) async fn rebind_session_ui(&mut self) {
        // V16-05 FR-B R3: the fresh session starts `Default`; when the
        // outgoing session was in Plan, subscribers get the
        // `plan → default` transition after the new session's extensions
        // are bound (below) so plan-mode extensions can drop their state.
        let previous_permission_mode = self.session.permission_mode();
        let session = self.runtime.session().clone();
        self.session = session.clone();

        // Detach the old session's subscription first
        // (interactive-mode.ts:1734-1735).
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }

        // `this.programStatus.reset()` (interactive-mode.ts:2095): forget
        // the outgoing session's run, then report the resting idle status.
        self.ui_state.reset_program_status();

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
        // `resetExtensionUI` (interactive-mode.ts:2452-2472): drop the
        // outgoing session's declarative autocomplete sources and rebuild the
        // provider before the fresh session's extensions re-register
        // (V17-11 FR-B R4).
        self.ui_state.reset_extension_autocomplete();
        self.ui_state.update_pending_messages_display();
        self.ui_state.render_initial_messages();

        // Subscribe AFTER rendering (upstream `renderBeforeBind: true`,
        // interactive-mode.ts:1739-1742) so no event is missed between the
        // rebuild and the subscription.
        self.unsubscribe = Some(self.ui_state.subscribe_to_agent());

        // The rest of `bindCurrentSessionExtensions` (interactive-mode.ts
        // :1817-1892) re-attaches the extension UI context
        // (`bindExtensions({ uiContext, mode: "tui" })`, :1818-1821) and the
        // extension shortcuts (:1889) on EVERY session switch. rpi's T15 W4
        // shape keeps `set_ui` on the host, and each replaced session owns a
        // fresh host (app.rs `create_runtime`), so both must be re-attached
        // here — without this the new host has no UI bridge and extension
        // status-bar entries (extension `setStatus` keys) and widgets
        // silently no-op after `/new` / `/resume` / `/clone` / `/fork` /
        // `/import`.
        //
        // ORDERING (rc.2, upstream parity): the UI attach must run BEFORE
        // the `bind_extensions` call — upstream binds the two atomically
        // (`bindExtensions({ uiContext, mode: "tui" })`), and the
        // `session_start` dispatched by `bind_extensions` must see the UI
        // (same fix as `init`; the inverted order was found in v0.1.5-rc.1
        // field verification via rpi-todo's never-claimed overlay).
        {
            let runner = session.extension_runner();
            if let Some(host) = runner
                .as_any()
                .and_then(|any| {
                    any.downcast_ref::<crate::core::extension_host_adapter::ExtensionHostAdapter>()
                })
                .map(|adapter| adapter.host().clone())
            {
                let bridge = lock(&self.ui_state.extension_ui_bridge)
                    .clone()
                    .unwrap_or_else(|| {
                        let bridge =
                            Arc::new(super::ui_bridge::InteractiveUiBridge::new(&self.ui_state));
                        *lock(&self.ui_state.extension_ui_bridge) = Some(Arc::clone(&bridge));
                        bridge
                    });
                host.set_ui(Some(bridge), rpi_ext_host::types::ExtensionMode::Tui);
                // `bindCommandContext` (agent-session.ts:3338): the fresh
                // session's host needs the interactive `ctx.command.*`
                // actions too (V17-11 FR-B R2).
                host.runtime().set_command_actions(Some(Arc::new(
                    super::InteractiveCommandActions::new(&self.ui_state),
                )));
            }
        }

        // `bindCurrentSessionExtensions` (interactive-mode.ts:1744) — the
        // extension bindings fire `session_start` with the UI attached.
        // V16-05: re-assert `ExtensionMode::Interactive` on every session
        // switch (the permission-mode gate keys on it).
        session
            .bind_extensions(ExtensionBindings {
                mode: Some(crate::core::extensions::ExtensionMode::Interactive),
                on_error: None,
                shutdown: Some({
                    let shutdown_tx = self.ui_state.shutdown_tx.clone();
                    std::sync::Arc::new(move || {
                        let _ = shutdown_tx.send(true);
                    })
                }),
            })
            .await;

        // `setupExtensionShortcuts(extensionRunner)` (interactive-mode.ts:1889)
        // — the hook consults the CURRENT session's runner.
        crate::modes::interactive::extension_shortcuts::install_extension_shortcuts(
            &self.ui_state,
            &session,
        );

        // V16-05 FR-B R3: dispatch the session-reset `mode_change` (only
        // when the outgoing session was in Plan). The built-in footer reads
        // the reset state directly, so it needs no separate invalidation.
        if previous_permission_mode != crate::core::permission_mode::PermissionMode::Default {
            session.notify_permission_mode_reset(previous_permission_mode);
        }

        self.ui_state.update_available_provider_count();
        self.ui_state.update_editor_border_color();
        self.ui_state.update_terminal_title();
    }
}

impl InteractiveUi {
    /// `applyRuntimeSettings` (interactive-mode.ts:1937-1961 @ 9841914):
    /// re-apply the (possibly new project's) settings to the footer, the
    /// data provider and the editor after a session switch. Includes the
    /// fullscreen scrollbar + copyOnSelect refreshes
    /// (`applyFullscreenScrollbarSetting` / `setCopyOnSelect`, :1940-1945 —
    /// the scrollbar line predates this task but was never ported; V14-16
    /// closes the gap alongside its own copyOnSelect line) and the
    /// `configureHttpDispatcher(getHttpIdleTimeoutMs())` refresh
    /// (:2063); the `!clearOnShrink` status-container clear is folded into
    /// the rebind's full render-state reset.
    fn apply_runtime_settings(&self, session: &AgentSession) {
        // `configureHttpDispatcher(this.settingsManager.getHttpIdleTimeoutMs())`
        // (interactive-mode.ts:2063): re-apply the (possibly new project's)
        // idle budget to the live dispatcher slot the stream function reads.
        if let Ok(timeout_ms) = session.settings_manager(|s| s.get_http_idle_timeout_ms()) {
            session.configure_http_dispatcher(timeout_ms);
        }
        // `setCapabilityOverrides(this.settingsManager.getTerminalCapabilityOverrides())`
        // (interactive-mode.ts:1940 @ 9841914, e86823096 / #8665): refresh
        // the capability overrides first — the rebind re-renders with the
        // current `terminal.*` settings.
        rpi_tui::terminal_image::set_capability_overrides(
            session.settings_manager(|settings| settings.get_terminal_capability_overrides()),
        );
        // `this.ensurePngTranscoder()` (interactive-mode.ts:2062 @
        // a276dabe5, #10292).
        self.ensure_png_transcoder();
        let manager = session.session_manager();
        let cwd = lock(&manager).get_cwd().to_path_buf();
        drop(manager);
        {
            let mut footer = lock(&self.footer);
            footer.set_session(session.clone());
            footer.set_auto_compact_enabled(session.auto_compaction_enabled());
        }
        // applyFullscreenScrollbarSetting (interactive-mode.ts:1894-1896 →
        // 1940): the transcript scroll view keeps the setting across
        // rebinds; the TuiHandle dispatch covers a live alt-screen renderer.
        let scrollbar = session.settings_manager(|s| s.get_fullscreen_scrollbar());
        self.apply_fullscreen_scrollbar(scrollbar);
        self.ui.set_fullscreen_scrollbar(scrollbar);
        // `if (this.renderer instanceof TuiAltScreen)` (interactive-mode.ts:
        // 1943-1945): runtime copyOnSelect refresh on the fullscreen
        // renderer only.
        if self.ui.mode() == rpi_tui::tui::TuiMode::Fullscreen {
            let copy_on_select = session.settings_manager(|s| s.get_fullscreen_copy_on_select());
            self.ui.set_copy_on_select(copy_on_select);
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

    /// `app.mode.cycle` (V16-05 FR-B R1; rpi-own): cycle `Default` ↔ `Plan`
    /// through the session's single authority, refresh the footer badge and
    /// confirm in the status line. The `mode_change` extension event is
    /// dispatched by [`AgentSession::cycle_permission_mode`].
    pub(crate) fn cycle_permission_mode(&self) {
        let mode = self.session().cycle_permission_mode();
        Component::invalidate(&mut *lock(&self.footer));
        self.show_status(&format!("Mode: {}", mode.as_str()));
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
                    ui.maybe_warn_about_anthropic_subscription_auth(Some(result.model))
                        .await;
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
            self.ui.stop(TuiStopOptions::default());
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

    /// `handleClipboardPaste` (interactive-mode.ts:3152-3192): the
    /// three-stage chain — file paths, images, then plain text.
    /// Image paths are inserted by absolute path; the temp file is
    /// intentionally left in place (upstream never removes it either).
    /// Dispatch-side entry: deferred to the drain (insertion touches the
    /// editor, which is locked during the dispatch).
    pub(crate) fn handle_paste_image(&self) {
        self.handle_paste_image_impl();
    }

    /// `handleClipboardPaste` (interactive-mode.ts:3152-3192) — synchronous
    /// form used by tests and direct callers. Production dispatch reads on a
    /// background thread and applies the payload via `ClipboardPasteReady`.
    pub(crate) fn handle_paste_image_impl(&self) {
        self.apply_clipboard_paste(read_clipboard_paste());
    }

    /// Dispatch-side entry: read the clipboard off the TUI thread. The read
    /// can block for seconds (PowerShell cold start, xclip selection wait), so
    /// it must not run in the drain. The resulting payload is queued as a
    /// normal `UiCommand`; insertion stays on the drain thread.
    pub(crate) fn start_clipboard_paste(&self) {
        if self.clipboard_paste_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        let queue = Arc::clone(&self.event_queue);
        let render_handle = self.render_handle.clone();
        let in_flight = Arc::clone(&self.clipboard_paste_in_flight);
        std::thread::spawn(move || {
            let paste = read_clipboard_paste();
            lock(&queue).push_back(UiCommand::ClipboardPasteReady(paste));
            render_handle.request_render();
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// Apply an already-read payload to the editor. Runs on the drain thread.
    pub(crate) fn apply_clipboard_paste(&self, paste: ClipboardPaste) {
        match paste {
            ClipboardPaste::Files(file_paths) => {
                if file_paths
                    .iter()
                    .any(|path| path.chars().any(char::is_control))
                {
                    self.show_status(
                        "Failed to paste from clipboard: Clipboard file path contains control characters",
                    );
                    return;
                }
                // `paths = isBashMode ? filePaths.map(quoteIfNeeded).join(" ")
                // : filePaths.join("\n")` (interactive-mode.ts:3159).
                let text = if *lock(&self.is_bash_mode) {
                    file_paths
                        .iter()
                        .map(|path| super::quote_if_needed(path))
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    file_paths.join("\n")
                };
                let text = self.with_cursor_spacing(&text);
                lock(&self.editor).insert_text_at_cursor(&text);
                self.render_handle.request_render();
            }
            ClipboardPaste::Image { bytes, mime_type } => {
                // `extensionForImageMimeType(image.mimeType) ?? "png"`
                // (interactive-mode.ts:3174).
                let extension = extension_for_image_mime_type(&mime_type).unwrap_or("png");
                if let Ok(file_path) = write_clipboard_image(&bytes, extension) {
                    lock(&self.editor).insert_text_at_cursor(&file_path.display().to_string());
                    self.render_handle.request_render();
                } else {
                    self.show_status("Failed to write clipboard image");
                }
            }
            ClipboardPaste::Text(text) => {
                lock(&self.editor).insert_text_at_cursor(&text);
                self.render_handle.request_render();
            }
            ClipboardPaste::Empty => self.show_status(
                "Clipboard is empty or no clipboard tool found (xclip / wl-paste / xsel / pbpaste / Get-Clipboard)",
            ),
        }
    }

    /// Cursor-aware spacing (interactive-mode.ts:3160-3165): surround the
    /// pasted text with a space when the cursor sits directly between
    /// non-whitespace characters.
    fn with_cursor_spacing(&self, text: &str) -> String {
        let editor = lock(&self.editor);
        let (line, col) = editor.get_cursor();
        let content = editor.get_text();
        let current_line = content
            .split('\n')
            .nth(line)
            .unwrap_or_default()
            .to_string();
        drop(editor);
        apply_cursor_spacing(text, &current_line, col)
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

/// Upstream `runClipboardCommand` default timeout (`options?.timeoutMs ??
/// 3000`, clipboard-command.ts:30) — caps how long a clipboard read may
/// block (xclip `-o` waits for the selection owner, which can hang on an
/// empty clipboard).
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_millis(3000);
/// Upstream `DEFAULT_LIST_TIMEOUT_MS` (clipboard-image.ts:19): the MIME
/// listing probes answer immediately when a clipboard owner exists.
const CLIPBOARD_LIST_TIMEOUT: Duration = Duration::from_millis(1000);
/// Upstream `DEFAULT_POWERSHELL_TIMEOUT_MS` (clipboard-image.ts:20).
const CLIPBOARD_POWERSHELL_TIMEOUT: Duration = Duration::from_millis(5000);
/// Upstream `maxBufferBytes` default (`options?.maxBufferBytes ?? 50 * 1024
/// * 1024`, clipboard-command.ts:38): a larger payload aborts the read
/// and resolves `undefined`.
const CLIPBOARD_MAX_BUFFER_BYTES: usize = 50 * 1024 * 1024;

/// Upstream `SUPPORTED_IMAGE_MIME_TYPES` (clipboard-image.ts:17).
const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Run a command capturing stdout, bounded by `timeout` and by the
/// upstream max-buffer cap (clipboard-command.ts:30/38). A reader thread
/// drains the pipe so large payloads cannot deadlock the child against
/// the buffer; past the cap it stops reading and the call yields `None`
/// (upstream aborts the child and resolves `undefined`). `None` = spawn
/// failure, timeout, non-zero exit or oversized output. Also the
/// subprocess seam of the tmux keyboard check (`tmux_check`).
pub(super) fn run_clipboard_command(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<Vec<u8>> {
    run_clipboard_command_with_cap(program, args, timeout, CLIPBOARD_MAX_BUFFER_BYTES)
}

/// [`run_clipboard_command`] with the buffer cap injected for tests
/// (production passes [`CLIPBOARD_MAX_BUFFER_BYTES`]).
fn run_clipboard_command_with_cap(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_buffer_bytes: usize,
) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (reader_tx, reader_rx) = std::sync::mpsc::channel();
    // Detached on purpose: a command that leaves a descendant holding the
    // pipe must not block the caller forever. The bounded receive below
    // keeps the UI responsive; a truly stuck reader thread is abandoned
    // rather than joined.
    let _reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut overflow = false;
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if bytes.len() + read > max_buffer_bytes {
                        overflow = true;
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                Err(_) => break,
            }
        }
        let _ = reader_tx.send(if overflow { None } else { Some(bytes) });
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
    let bytes = reader_rx
        .recv_timeout(Duration::from_millis(500))
        .ok()
        .flatten();
    status.filter(|status| status.success()).and(bytes)
}

/// The cursor-aware spacing rule (interactive-mode.ts:3160-3165): a space
/// is added on each side whose neighbour character is non-whitespace.
fn apply_cursor_spacing(text: &str, current_line: &str, col: usize) -> String {
    let before = if col > 0 {
        current_line.chars().nth(col - 1)
    } else {
        None
    };
    let after = current_line.chars().nth(col);
    let leading = before.is_some_and(|c| !c.is_whitespace());
    let trailing = after.is_some_and(|c| !c.is_whitespace());
    format!(
        "{}{}{}",
        if leading { " " } else { "" },
        text,
        if trailing { " " } else { "" }
    )
}

/// The `ClipboardImage` shape (clipboard-image.ts:12-15).
struct ClipboardImage {
    bytes: Vec<u8>,
    mime_type: String,
}

/// `baseMimeType` (clipboard-image.ts:26-28): strips parameters and
/// lowercases.
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_lowercase()
}

/// `extensionForImageMimeType` (clipboard-image.ts:30-43).
fn extension_for_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// `isSupportedImageMimeType` (clipboard-image.ts:62-65).
fn is_supported_image_mime_type(mime_type: &str) -> bool {
    let base = base_mime_type(mime_type);
    SUPPORTED_IMAGE_MIME_TYPES
        .iter()
        .any(|supported| *supported == base)
}

/// `selectPreferredImageMimeType` (clipboard-image.ts:45-60): the first
/// supported type in the fixed preference order, else any `image/*`.
fn select_preferred_image_mime_type(mime_types: &str) -> Option<String> {
    let normalized: Vec<(String, String)> = mime_types
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|raw| (raw.to_string(), base_mime_type(raw)))
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some((raw, _)) = normalized.iter().find(|(_, base)| base == preferred) {
            return Some(raw.clone());
        }
    }
    normalized
        .iter()
        .find(|(_, base)| base.starts_with("image/"))
        .map(|(raw, _)| raw.clone())
}

/// The file-list flavours the Linux command-line readers probe, in
/// preference order. GNOME writes `x-special/gnome-copied-files` with a
/// `copy`/`cut` header line; KDE and most other toolkits use
/// `text/uri-list` (RFC 2483). Upstream reads `getFilePaths()` from the
/// native helper instead (D-104/D-106).
const FILE_LIST_MIME_TYPES: [&str; 2] = ["text/uri-list", "x-special/gnome-copied-files"];

/// Picks the first advertised file-list flavour.
fn select_file_list_mime_type(types: &str) -> Option<&'static str> {
    let advertised: Vec<String> = types
        .lines()
        .map(|line| line.trim().to_lowercase())
        .collect();
    FILE_LIST_MIME_TYPES
        .iter()
        .find(|mime| advertised.iter().any(|advertised| advertised == *mime))
        .copied()
}

/// Parses an RFC 2483 `text/uri-list` (or the GNOME `copy`/`cut` variant)
/// into local paths; `url::Url::to_file_path` decodes percent escapes.
/// Non-`file:` entries and non-UTF-8 payloads are ignored (`None` when no
/// path survives).
fn parse_file_uri_list(bytes: &[u8], mime_type: &str) -> Option<Vec<String>> {
    let text = String::from_utf8(bytes.to_vec()).ok()?;
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if mime_type == "x-special/gnome-copied-files" && (line == "copy" || line == "cut") {
            continue;
        }
        let Ok(url) = url::Url::parse(line) else {
            continue;
        };
        if url.scheme() != "file" {
            continue;
        }
        let Ok(path) = url.to_file_path() else {
            continue;
        };
        paths.push(path.display().to_string());
    }
    if paths.is_empty() { None } else { Some(paths) }
}

/// `isWaylandSession` (clipboard-image.ts:22-25): `WAYLAND_DISPLAY` or
/// `XDG_SESSION_TYPE=wayland`. The text read already uses this same double
/// predicate (`read_clipboard_text`); the expression is repeated because
/// that function is frozen (V17-10 FR-B red line).
fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|session| session == "wayland")
}

/// `isWSL` for the image orchestration (clipboard-image.ts:213-220) —
/// reused from the write-side env snapshot.
fn is_wsl_session() -> bool {
    crate::modes::interactive::clipboard::ClipboardEnv::from_env().wsl
}

/// The file-path stage of `handleClipboardPaste`
/// (interactive-mode.ts:3154-3169): upstream delegates to the native
/// `getFilePaths()`; rpi has no native helper (D-104/D-106), so the
/// platform command-line equivalents are probed instead. `None` = the
/// clipboard holds no file list.
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        read_clipboard_file_paths_linux()
    }
    #[cfg(target_os = "macos")]
    {
        read_clipboard_file_paths_macos()
    }
    #[cfg(target_os = "windows")]
    {
        read_clipboard_file_paths_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Linux file-path stage: `text/uri-list` probing via wl-paste on
/// Wayland and xclip on X11 ([RPI-OWN] mechanism, D-106). Compiled on
/// every host so the platform readers stay stub-testable off-platform
/// (only the dispatch is cfg-gated).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_clipboard_file_paths_linux() -> Option<Vec<String>> {
    if is_wayland_session() {
        // A working Wayland clipboard is authoritative: a listing without
        // a file-list flavour does not fall through to the XWayland
        // clipboard (the same null-blocks-fallback rule as the image
        // reader); only a failed probe falls back, like the text read.
        let Some(bytes) =
            run_clipboard_command("wl-paste", &["--list-types"], CLIPBOARD_LIST_TIMEOUT)
        else {
            return read_clipboard_file_paths_x11();
        };
        let mime = select_file_list_mime_type(&String::from_utf8_lossy(&bytes));
        return mime.and_then(|mime| {
            run_clipboard_command(
                "wl-paste",
                &["--type", mime, "--no-newline"],
                CLIPBOARD_READ_TIMEOUT,
            )
            .and_then(|data| parse_file_uri_list(&data, mime))
        });
    }
    read_clipboard_file_paths_x11()
}

/// The X11 file-list probe (also the Wayland fallback when the wl-paste
/// probe failed).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_clipboard_file_paths_x11() -> Option<Vec<String>> {
    if let Some(bytes) = run_clipboard_command(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        CLIPBOARD_LIST_TIMEOUT,
    ) && let Some(mime) = select_file_list_mime_type(&String::from_utf8_lossy(&bytes))
        && let Some(data) = run_clipboard_command(
            "xclip",
            &["-selection", "clipboard", "-t", mime, "-o"],
            CLIPBOARD_READ_TIMEOUT,
        )
        && let Some(paths) = parse_file_uri_list(&data, mime)
    {
        return Some(paths);
    }
    None
}

/// macOS file-path stage: the `getFilePaths()` command-line equivalent
/// (AppleScript coerces the pasteboard to file URLs).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn read_clipboard_file_paths_macos() -> Option<Vec<String>> {
    let bytes = run_clipboard_command(
        "osascript",
        &["-e", MACOS_FILE_PATHS_SCRIPT],
        CLIPBOARD_READ_TIMEOUT,
    )?;
    let text = String::from_utf8_lossy(&bytes);
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if paths.is_empty() { None } else { Some(paths) }
}

/// POSIX paths of file URLs on the clipboard, one per line
/// (`getFilePaths()` equivalent). A non-file clipboard leaves the list
/// empty and returns nothing.
const MACOS_FILE_PATHS_SCRIPT: &str = r#"
set theItems to {}
try
	set theItems to (the clipboard as «class furl»)
end try
if class of theItems is not list then set theItems to {theItems}
set output to ""
repeat with theItem in theItems
	try
		set output to output & POSIX path of theItem & linefeed
	end try
end repeat
return output
"#;

/// Windows clipboard scripts. `[Console]::Out` avoids PowerShell's table
/// formatter (a bare `FileInfo` object would print Mode/LastWriteTime/Length/
/// Name columns) and `-Raw` keeps the clipboard text byte-for-byte instead of
/// letting the formatter append a newline.
const WINDOWS_CLIPBOARD_TEXT_SCRIPT: &str = "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::Out.Write((Get-Clipboard -Raw))";
const WINDOWS_CLIPBOARD_FILE_PATHS_SCRIPT: &str = "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); Get-Clipboard -Format FileDropList | ForEach-Object { [Console]::Out.WriteLine($_.FullName) }";

/// Windows file-path stage: `Get-Clipboard -Format FileDropList` (the
/// native `getFilePaths()` equivalent).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn read_clipboard_file_paths_windows() -> Option<Vec<String>> {
    let bytes = run_clipboard_command(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_CLIPBOARD_FILE_PATHS_SCRIPT,
        ],
        CLIPBOARD_READ_TIMEOUT,
    )?;
    let text = String::from_utf8_lossy(&bytes);
    let paths: Vec<String> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && looks_like_windows_absolute_path(line))
        .map(str::to_owned)
        .collect();
    if paths.is_empty() { None } else { Some(paths) }
}

/// `FileInfo.FullName` is always absolute. Filtering here keeps PowerShell
/// headers or error text from reaching the editor if the command regresses.
fn looks_like_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/'))
        || path.starts_with("\\\\")
}

/// `readClipboardImage` (clipboard-image.ts:200-239) — the reader
/// orchestration and the unsupported-format conversion. The native reader
/// (`:193`) has no Rust counterpart; each platform substitutes the
/// command-line equivalent (D-104/D-106): wl-paste / xclip on Linux
/// (PowerShell as the WSL fallback), PowerShell on Windows, osascript on
/// macOS.
fn read_clipboard_image() -> Option<ClipboardImage> {
    // `if (env.TERMUX_VERSION) return null` (clipboard-image.ts:207-209).
    if std::env::var_os("TERMUX_VERSION").is_some_and(|value| !value.is_empty()) {
        return None;
    }

    #[cfg(target_os = "linux")]
    let image = read_clipboard_image_linux();
    #[cfg(target_os = "windows")]
    let image = read_clipboard_image_via_power_shell();
    #[cfg(target_os = "macos")]
    let image = read_clipboard_image_via_osascript();
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let image = None;

    let image = image?;
    // Convert unsupported formats (e.g. Windows DIB data wrapped as BMP)
    // to PNG (clipboard-image.ts:230-237).
    if !is_supported_image_mime_type(&image.mime_type) {
        let png = convert_to_png(&image.bytes)?;
        return Some(ClipboardImage {
            bytes: png,
            mime_type: "image/png".to_string(),
        });
    }
    Some(image)
}

/// Linux image orchestration (clipboard-image.ts:213-221): wl-paste on
/// Wayland/WSL, xclip next, then the PowerShell fallback on WSL. The
/// tri-state mirrors upstream: outer `None` = backend failed
/// (`undefined`), `Some(None)` = no image (`null`); a `null` from wl-paste
/// blocks the xclip fallback, preserving the empty-Wayland-clipboard
/// semantics. Compiled on every host (see `read_clipboard_image`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_clipboard_image_linux() -> Option<ClipboardImage> {
    let wsl = is_wsl_session();
    let mut image: Option<Option<ClipboardImage>> = None;
    if is_wayland_session() || wsl {
        image = read_clipboard_image_via_wl_paste();
    }
    if image.is_none() {
        image = read_clipboard_image_via_xclip();
    }
    // `if (!image && wsl) image = (await readClipboardImageViaPowerShell())
    // ?? image` (clipboard-image.ts:220): a null/failed PowerShell result
    // keeps the prior state.
    if !matches!(image, Some(Some(_)))
        && wsl
        && let Some(png) = read_clipboard_image_via_power_shell()
    {
        image = Some(Some(png));
    }
    image.flatten()
}

/// `readClipboardImageViaWlPaste` (clipboard-image.ts:91-111).
fn read_clipboard_image_via_wl_paste() -> Option<Option<ClipboardImage>> {
    let list = run_clipboard_command("wl-paste", &["--list-types"], CLIPBOARD_LIST_TIMEOUT)?;
    // A successful listing without an image is upstream `null` (no image),
    // not `undefined` (backend failed): it blocks the xclip fallback.
    let Some(selected) = select_preferred_image_mime_type(&String::from_utf8_lossy(&list)) else {
        return Some(None);
    };
    let data = run_clipboard_command(
        "wl-paste",
        &["--type", &selected, "--no-newline"],
        CLIPBOARD_READ_TIMEOUT,
    )?;
    if data.is_empty() {
        return Some(None);
    }
    Some(Some(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&selected),
    }))
}

/// `readClipboardImageViaXclip` (clipboard-image.ts:172-191): list the
/// advertised TARGETS first and only read a preferred image MIME
/// (#9786 `8676a0dcd`).
fn read_clipboard_image_via_xclip() -> Option<Option<ClipboardImage>> {
    let targets = run_clipboard_command(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        CLIPBOARD_LIST_TIMEOUT,
    )?;
    // Advertised targets without an image = upstream `null` (#9786).
    let Some(selected) = select_preferred_image_mime_type(&String::from_utf8_lossy(&targets))
    else {
        return Some(None);
    };
    let data = run_clipboard_command(
        "xclip",
        &["-selection", "clipboard", "-t", &selected, "-o"],
        CLIPBOARD_READ_TIMEOUT,
    )?;
    if data.is_empty() {
        return Some(None);
    }
    Some(Some(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&selected),
    }))
}

/// The Windows/WSL image reader: `Get-Clipboard -Format Image` through
/// PowerShell (`readClipboardImageViaPowerShell`, clipboard-image.ts:
/// 118-170). Upstream streams the bitmap to a temp file; the port returns
/// it as base64 on stdout — the same bytes without the `wslpath` round
/// trip (D-106). `None` = no image or failure.
fn read_clipboard_image_via_power_shell() -> Option<ClipboardImage> {
    let script = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img) { $ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); [Console]::Out.Write([Convert]::ToBase64String($ms.ToArray())) }";
    let bytes = run_clipboard_command(
        "powershell.exe",
        &["-NoProfile", "-Command", script],
        CLIPBOARD_POWERSHELL_TIMEOUT,
    )?;
    let encoded = String::from_utf8_lossy(&bytes);
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return None;
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .ok()?;
    if decoded.is_empty() {
        return None;
    }
    Some(ClipboardImage {
        bytes: decoded,
        mime_type: "image/png".to_string(),
    })
}

/// macOS image reader: the native `getImage()` command-line equivalent.
/// AppleScript writes the pasteboard image to a temp file and returns the
/// class token the MIME is derived from (PNG → JPEG → GIF, the upstream
/// `detectSupportedImageMimeType` coverage). Compiled on every host so a
/// stub `osascript` can exercise it on the test host.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn read_clipboard_image_via_osascript() -> Option<ClipboardImage> {
    let (path, file) = create_private_temp_file("img").ok()?;
    drop(file);
    let script = format!(
        r#"set outPath to "{}"
try
	set d to (the clipboard as «class PNGf»)
	set f to open for access POSIX file outPath with write permission
	write d to f
	close access f
	return "png"
end try
try
	set d to (the clipboard as «class JPEG»)
	set f to open for access POSIX file outPath with write permission
	write d to f
	close access f
	return "jpeg"
end try
try
	set d to (the clipboard as «class GIFf»)
	set f to open for access POSIX file outPath with write permission
	write d to f
	close access f
	return "gif"
end try
return ""
"#,
        applescript_string(&path.display().to_string())
    );
    let token = run_clipboard_command("osascript", &["-e", &script], CLIPBOARD_READ_TIMEOUT)
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default();
    let mime_type = match token.as_str() {
        "png" => "image/png",
        "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    let bytes = bytes.ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(ClipboardImage {
        bytes,
        mime_type: mime_type.to_string(),
    })
}

/// Escape a value for an AppleScript string literal.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `convertToPng` (clipboard-image.ts:71-87): the Photon WASM converter is
/// replaced by the workspace's `image` crate (already a dependency of the
/// read tool). `None` when the bytes cannot be decoded or re-encoded.
fn convert_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let format = image::guess_format(bytes).ok()?;
    let decoded = image::load_from_memory_with_format(bytes, format).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    decoded.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// The three-stage read (`handleClipboardPaste`): files → image → text.
/// Runs off the drain thread; the caller never sees partial failures.
fn read_clipboard_paste() -> ClipboardPaste {
    if let Some(file_paths) = read_clipboard_file_paths() {
        return ClipboardPaste::Files(file_paths);
    }
    if let Some(image) = read_clipboard_image() {
        return ClipboardPaste::Image {
            bytes: image.bytes,
            mime_type: image.mime_type,
        };
    }
    match read_clipboard_text() {
        Some(text) if !text.is_empty() => ClipboardPaste::Text(text),
        _ => ClipboardPaste::Empty,
    }
}

/// `readClipboardText` (clipboard.ts:36-47) — the native clipboard addon is
/// replaced by the platform tools: `wl-paste` / `xclip` / `xsel` on Linux,
/// `Get-Clipboard -Raw` on Windows, `pbpaste` on macOS.
fn read_clipboard_text() -> Option<String> {
    // #10391 (592fb57b7): Termux reports platform "android", not "linux",
    // so `termux-clipboard-get` runs before the Linux-only branch. An empty
    // read is treated as no clipboard, like upstream `bytes.toString() || null`.
    if std::env::var_os("TERMUX_VERSION").is_some()
        && let Some(bytes) =
            run_clipboard_command("termux-clipboard-get", &[], CLIPBOARD_READ_TIMEOUT)
        && let Ok(text) = String::from_utf8(bytes)
        && !text.is_empty()
    {
        return Some(text);
    }
    #[cfg(target_os = "linux")]
    {
        read_clipboard_text_linux()
    }
    #[cfg(target_os = "windows")]
    {
        read_clipboard_text_windows()
    }
    #[cfg(target_os = "macos")]
    {
        run_clipboard_command("pbpaste", &[], CLIPBOARD_READ_TIMEOUT)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .filter(|text| !text.is_empty())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// Linux text read. Upstream tries `wl-paste`, then `xclip`, then `xsel`;
/// an empty result falls through to the next command.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_clipboard_text_linux() -> Option<String> {
    let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
    if is_wayland_session() {
        commands.push(("wl-paste", vec!["--no-newline", "--type", "text"]));
    }
    commands.push(("xclip", vec!["-selection", "clipboard", "-o"]));
    commands.push(("xsel", vec!["--clipboard", "--output"]));
    for (program, args) in commands {
        if let Some(bytes) = run_clipboard_command(program, &args, CLIPBOARD_READ_TIMEOUT) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Windows text read. `-Raw` preserves embedded newlines; `[Console]::Out.Write`
/// avoids the formatter's trailing newline.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn read_clipboard_text_windows() -> Option<String> {
    let bytes = run_clipboard_command(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_CLIPBOARD_TEXT_SCRIPT,
        ],
        CLIPBOARD_READ_TIMEOUT,
    )?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.is_empty() { None } else { Some(text) }
}

/// Create a private, randomly named temp file. `create_new` refuses to follow
/// a pre-planted symlink; mode 0600 keeps clipboard images (screenshots) out of
/// other local users' reach even though the file outlives the paste, as
/// upstream does.
fn create_private_temp_file(
    extension: &str,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    let path = std::env::temp_dir().join(format!(
        "rpi-clipboard-{}.{extension}",
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    Ok((path, file))
}

fn write_clipboard_image(bytes: &[u8], extension: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    let (path, mut file) = create_private_temp_file(extension)?;
    file.write_all(bytes)?;
    Ok(path)
}

// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rpi_agent::messages::AgentMessage;
    use rpi_ai::types::{UserContent, UserMessage, UserRole};

    use super::*;
    use crate::core::agent_session::AgentSession;
    use crate::core::agent_session_runtime::{
        AgentSessionRuntime, CreateAgentSessionRuntimeResult, CreateRuntimeOptions,
    };
    use crate::core::agent_session_services::{
        CreateAgentSessionServicesOptions, create_agent_session_services,
    };
    use crate::core::model_resolver::find_exact_model_reference_match;
    use crate::core::session_manager::NewSessionOptions;
    use crate::modes::interactive::interactive_mode::InteractiveModeOptions;
    use crate::modes::interactive::test_support::{
        TempDir, TestSession, TestTerminal, build_test_session, build_test_session_with,
    };
    use crate::sdk::{CreateAgentSessionOptions, NoTools, create_agent_session};

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
        switchable_harness_inner(None, false).await
    }

    /// Like [`switchable_harness`] but the session manager persists into a
    /// temp session directory. Import tests need a real destination dir: the
    /// in-memory manager's empty session dir would copy imported files next
    /// to the process cwd.
    async fn switchable_harness_with_session_dir()
    -> (InteractiveMode, Arc<TestTerminal>, AgentSession, TempDir) {
        switchable_harness_inner(None, true).await
    }

    /// Like [`switchable_harness`] but the session factory always fails with
    /// the given message, so runtime-dependent commands hit their error
    /// branches.
    async fn failing_factory_harness(
        factory_error: &str,
    ) -> (InteractiveMode, Arc<TestTerminal>, AgentSession, TempDir) {
        switchable_harness_inner(Some(factory_error.to_owned()), false).await
    }

    async fn switchable_harness_inner(
        factory_error: Option<String>,
        persistent_sessions: bool,
    ) -> (InteractiveMode, Arc<TestTerminal>, AgentSession, TempDir) {
        install_global_keybindings();
        let tmp = TempDir::new();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(
            agent_dir.join("models.json"),
            // `${VAR}` reference: the harness wants the provider unavailable
            // unless a test exports the var (see test_support.rs).
            r#"{"providers": {"custom": {
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "${RPI_TEST_INTERACTIVE_KEY}",
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

        let session_manager = if persistent_sessions {
            let sessions = tmp.path().join("sessions");
            std::fs::create_dir_all(&sessions).expect("sessions dir");
            let file = sessions.join("s.jsonl");
            Arc::new(Mutex::new(
                SessionManager::open(&file, Some(&sessions), None).expect("session manager"),
            ))
        } else {
            Arc::new(Mutex::new(
                SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
                    .expect("in-memory session manager"),
            ))
        };

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
            use crate::RpiError;
            use futures::future::BoxFuture;
            match factory_error {
                Some(message) => Arc::new(move |_options: CreateRuntimeOptions| {
                    let message = message.clone();
                    Box::pin(async move { Err(RpiError::Session(message.clone())) })
                        as BoxFuture<'static, Result<CreateAgentSessionRuntimeResult, RpiError>>
                })
                    as crate::core::agent_session_runtime::CreateAgentSessionRuntimeFactory,
                None => {
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
                            as BoxFuture<'static, Result<CreateAgentSessionRuntimeResult, RpiError>>
                    })
                }
            }
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
        let _ = session
            .model_runtime()
            .set_runtime_api_key("custom", "sk-test-interactive")
            .await;
    }

    // ---------------------------------------------------------------------
    // Radius selection immediacy (V15-03 FR-C / R2.3.3, 4d38031fb +
    // 9767ba275)
    // ---------------------------------------------------------------------

    /// FR-C: `balanced` resolves from the static public catalog alone — the
    /// post-login default selection needs no gateway request on the
    /// selection path (`finish_authentication` performs no refresh; the
    /// radius provider's baseline comes from the vendored catalog).
    #[tokio::test]
    async fn login_completion_selects_radius_balanced_from_static_catalog() {
        let (mut mode, _terminal, session, _tmp) = mode_harness().await;
        mode.init().await;
        // Configure radius with an in-memory API key (no gateway round-trip;
        // the scoped post-credential refresh is offline by construction).
        session
            .model_runtime()
            .set_runtime_api_key("radius", "radius-test-key")
            .await
            .expect("set runtime api key");
        let ui = Arc::clone(&mode.ui_state);
        super::finish_authentication(
            &ui,
            session.model_runtime(),
            "radius",
            "Logged in to Radius",
            None,
        )
        .await
        .expect("finish authentication");
        mode.ui_state.ui.tick(Instant::now());
        let chat = rendered_chat(&mode.ui_state);
        assert!(
            chat.contains("Logged in to Radius. Selected balanced"),
            "chat: {chat}"
        );
        assert_eq!(
            mode.ui_state.session().model().map(|model| model.id),
            Some("balanced".to_string())
        );
        // The selection is a static-catalog model, not a gateway entry.
        let selected = mode.ui_state.session().model().expect("model");
        assert_eq!(selected.base_url, "https://radius.pi.dev/v1");
    }

    /// `deferSelection` (interactive-mode.ts:5681-5685 @ 9767ba275): waits
    /// for catalog discovery only when the default model is missing from
    /// the snapshot — with the static Radius catalog `balanced` is present
    /// from the start, so selection stays immediate.
    #[test]
    fn post_login_selection_defer_predicate_matches_upstream() {
        let radius_model = |id: &str| Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: rpi_ai::types::ApiKind::from("rpi-messages"),
            provider: "radius".to_owned(),
            base_url: "https://radius.pi.dev/v1".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            input_limits: None,
            cost: Default::default(),
            prompt_cache: None,
            context_window: 1000,
            max_tokens: 100,
            headers: None,
            compat: None,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
        };
        // Unknown placeholder + empty snapshot → defer (the account-catalog
        // case: "Refreshing model catalog…" until discovery completes).
        assert!(post_login_selection_should_defer(None, "radius", &[]));
        // Static catalog already lists `balanced` → immediate.
        assert!(!post_login_selection_should_defer(
            None,
            "radius",
            &[radius_model("balanced")]
        ));
        // A different id does not satisfy the default → defer.
        assert!(post_login_selection_should_defer(
            None,
            "radius",
            &[radius_model("cheap")]
        ));
        // A selected model (no unknown placeholder) never defers.
        assert!(!post_login_selection_should_defer(
            Some(&radius_model("cheap")),
            "radius",
            &[]
        ));
        // Providers without a configured default never defer.
        assert!(!post_login_selection_should_defer(None, "llama.cpp", &[]));
    }

    fn rendered_chat(ui: &InteractiveUi) -> String {
        lock(&ui.chat_container).render(60).join("\n")
    }

    fn selector_mounted(ui: &InteractiveUi) -> bool {
        lock(&ui.active_selector).is_some()
    }

    /// Wait for the mounted built-in dialog carrying `needle`, then feed
    /// `keys` one per tick (V17-08 #63 built-in selector dialogs).
    async fn answer_mounted_dialog(
        ui: &Arc<InteractiveUi>,
        terminal: &Arc<TestTerminal>,
        needle: &str,
        keys: &[&str],
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mounted = lock(&ui.editor_container).render(120).join("\n");
            if mounted.contains(needle) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "dialog {needle:?} never mounted; content: {mounted}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        for key in keys {
            terminal.feed(key);
            ui.ui.tick(Instant::now());
        }
    }

    #[tokio::test]
    async fn session_selector_progressive_load_routes_through_drain() {
        // V15-04 FR-A (dfbf793b7): the session selector loads progressively
        // on loader threads and its publishes wake the run loop's
        // `UiCommand` drain, which applies them to the mounted component
        // through the typed slot.
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        InteractiveUi::show_session_selector(&mode.ui_state);
        assert!(selector_mounted(&mode.ui_state), "session selector mounted");

        // The typed slot is registered and draining settles the load.
        let selector = lock(&mode.ui_state.session_selector)
            .clone()
            .expect("typed session selector slot");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            mode.ui_state.drain_events();
            if !lock(&selector).loads_pending() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "loads never settled through the drain"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Hiding the selector clears the slot (stale drains become no-ops).
        mode.ui_state.hide_selector();
        assert!(lock(&mode.ui_state.session_selector).is_none());
        mode.ui_state
            .push(crate::modes::interactive::interactive_mode::UiCommand::SessionListEvent);
        mode.ui_state.drain_events();
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

    /// `ed8b3bcc1`: the top-level `/login` menu offers Radius last, with its
    /// status indicator.
    #[tokio::test]
    async fn login_auth_type_selector_offers_radius_last() {
        let (mut mode, _terminal, _session, _tmp) = mode_harness().await;
        mode.init().await;
        mode.ui_state.handle_login_command(None).await;
        let rendered = lock(&mode.ui_state.active_selector)
            .as_ref()
            .map(|entry| entry.lock().unwrap().render(70).join("\n"))
            .unwrap_or_default();
        assert!(
            rendered.contains("Sign in with Radius"),
            "radius row missing; rendered: {rendered}"
        );
        let radius_line = rendered
            .lines()
            .position(|line| line.contains("Sign in with Radius"));
        let last_sign_in = rendered
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("Sign in with"))
            .map(|(index, _)| index)
            .last();
        assert_eq!(
            radius_line, last_sign_in,
            "radius row must be the last option; rendered: {rendered}"
        );
        assert!(
            rendered.contains("not configured"),
            "status indicator missing; rendered: {rendered}"
        );
    }

    /// Restore `RPI_CODING_AGENT_DIR` on drop (test env hygiene).
    struct AgentDirEnv(Option<std::ffi::OsString>);
    impl Drop for AgentDirEnv {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => rpi_test_env::set_var("RPI_CODING_AGENT_DIR", value),
                None => rpi_test_env::remove_var("RPI_CODING_AGENT_DIR"),
            }
        }
    }

    /// `ed8b3bcc1`: confirming the Radius MCP offer writes the global
    /// `mcp.json` entry (URL + `auth.provider`) and triggers a reload.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn offer_radius_mcp_server_writes_global_config() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        let (mut mode, terminal, _session, tmp) = mode_harness().await;
        let agent_dir = tmp.path().join("agent");
        let _restore = AgentDirEnv(std::env::var_os("RPI_CODING_AGENT_DIR"));
        rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
        mode.init().await;

        offer_radius_mcp_server(&mode.ui_state, "radius", "Radius");
        assert!(
            selector_mounted(&mode.ui_state),
            "offer selector mounted; chat: {}",
            rendered_chat(&mode.ui_state)
        );
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());
        // The offer flow triggers `handleReloadCommand` synchronously in the
        // selector callback, which mounts the reload box (V17-08 #63) before
        // the background reload dismisses it.
        let mounted = lock(&mode.ui_state.editor_container).render(80).join("\n");
        assert!(
            mounted.contains("Reloading keybindings"),
            "reload box mounted by the radius offer flow: {mounted}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        let body = std::fs::read_to_string(agent_dir.join("mcp.json")).expect("mcp.json");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("mcp json");
        assert_eq!(
            parsed["mcpServers"]["radius"]["url"],
            serde_json::json!(crate::core::radius::radius_mcp_url())
        );
        assert_eq!(
            parsed["mcpServers"]["radius"]["auth"]["provider"],
            serde_json::json!("radius")
        );
    }

    /// `ed8b3bcc1`: a global Radius server already using this login asks
    /// nothing.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn offer_radius_mcp_server_skips_when_already_configured() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        let (mut mode, _terminal, _session, tmp) = mode_harness().await;
        let agent_dir = tmp.path().join("agent");
        let _restore = AgentDirEnv(std::env::var_os("RPI_CODING_AGENT_DIR"));
        rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
        std::fs::write(
            agent_dir.join("mcp.json"),
            serde_json::json!({"mcpServers": {"radius": {
                "url": crate::core::radius::radius_mcp_url(),
                "auth": {"provider": "radius"}
            }}})
            .to_string(),
        )
        .expect("write mcp.json");
        mode.init().await;

        offer_radius_mcp_server(&mode.ui_state, "radius", "Radius");
        assert!(
            !selector_mounted(&mode.ui_state),
            "no offer when already configured"
        );
    }

    /// `ed8b3bcc1`: a name collision with a different server shifts the new
    /// entry to `radius-mcp`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn offer_radius_mcp_server_avoids_a_name_collision() {
        let _env_guard = lock(&crate::modes::interactive::test_support::TEST_ENV_LOCK);
        let (mut mode, terminal, _session, tmp) = mode_harness().await;
        let agent_dir = tmp.path().join("agent");
        let _restore = AgentDirEnv(std::env::var_os("RPI_CODING_AGENT_DIR"));
        rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
        std::fs::write(
            agent_dir.join("mcp.json"),
            serde_json::json!({"mcpServers": {"radius": {"url": "https://other.example/mcp"}}})
                .to_string(),
        )
        .expect("write mcp.json");
        mode.init().await;

        offer_radius_mcp_server(&mode.ui_state, "radius", "Radius");
        assert!(selector_mounted(&mode.ui_state));
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(Duration::from_millis(100)).await;

        let body = std::fs::read_to_string(agent_dir.join("mcp.json")).expect("mcp.json");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("mcp json");
        assert_eq!(
            parsed["mcpServers"]["radius-mcp"]["url"],
            serde_json::json!(crate::core::radius::radius_mcp_url())
        );
        assert_eq!(
            parsed["mcpServers"]["radius"]["url"],
            serde_json::json!("https://other.example/mcp")
        );
    }

    /// `/login minimax-cn` must reach the api-key prompt dialog: the built-in
    /// provider (seeded since D-052) uses `env_api_key_auth`, whose login
    /// prompts for the key.
    /// Full user path: bare `/login` → auth-type selector → "Sign in with
    /// an API key" → provider list → pick `minimax-cn` → key prompt dialog.
    #[tokio::test]
    async fn login_bare_flow_to_api_key_prompt_via_input() {
        let (mut mode, terminal, _session, _tmp) = mode_harness().await;
        mode.init().await;
        mode.ui_state.handle_login_command(None).await;
        assert!(
            selector_mounted(&mode.ui_state),
            "auth-type selector mounted"
        );

        // Down + Enter selects "Sign in with an API key".
        terminal.feed("\x1b[B");
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Search narrows the api-key provider list to the two MiniMax rows.
        for ch in "minimax".chars() {
            terminal.feed(&ch.to_string());
        }
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // The fuzzy match puts "MiniMax" (intl) first; move down to the CN
        // row — the provider from the user report.
        terminal.feed("\x1b[B");
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let rendered = lock(&mode.ui_state.active_selector)
            .as_ref()
            .map(|entry| entry.lock().unwrap().render(60).join("\n"))
            .unwrap_or_default();
        assert!(
            rendered.contains("MiniMax CN"),
            "api-key provider list after auth-type pick + search; rendered: {rendered}"
        );

        // Enter confirms the highlighted row.
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let rendered = lock(&mode.ui_state.active_selector)
            .as_ref()
            .map(|entry| entry.lock().unwrap().render(60).join("\n"))
            .unwrap_or_default();
        assert!(
            rendered.contains("Enter MiniMax CN API key"),
            "key prompt dialog; rendered: {rendered}"
        );

        // Type the key and confirm; the credential is stored.
        terminal.feed("sk-minimax-test");
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let credentials = mode
            .ui_state
            .session()
            .model_runtime()
            .list_credentials()
            .await
            .expect("list credentials");
        assert!(
            credentials
                .iter()
                .any(|entry| entry.provider_id == "minimax-cn"
                    && entry.credential_type == CredentialType::ApiKey),
            "stored credential; got: {:?}",
            credentials
                .iter()
                .map(|entry| (entry.provider_id.as_str(), entry.credential_type))
                .collect::<Vec<_>>()
        );
    }

    /// The full post-Enter path must complete: store credential → refresh
    /// (incl. the remote-catalog overlay) → hide the dialog → status line.
    #[tokio::test]
    async fn login_enter_completes_flow_and_hides_dialog() {
        let (mut mode, terminal, _session, _tmp) = mode_harness().await;
        mode.init().await;
        mode.ui_state.handle_login_command(Some("minimax-cn")).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        mode.ui_state.ui.tick(std::time::Instant::now());

        terminal.feed("sk-minimax-test");
        terminal.feed("\r");

        // The spawned login task (store + refresh + complete) must finish:
        // the selector is hidden only after `ModelRuntime::login` returns.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            mode.ui_state.ui.tick(std::time::Instant::now());
            if !selector_mounted(&mode.ui_state) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "login flow did not complete: selector still mounted"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let chat = rendered_chat(&mode.ui_state);
        assert!(
            chat.contains("Saved API key for MiniMax CN"),
            "completion status; chat: {chat}"
        );
    }

    /// Regression: a SECOND `/login` for a provider whose credential is
    /// already stored must not hang (the real-app report: after the first
    /// successful login, `/login minimax-cn` froze the UI).
    #[tokio::test]
    async fn second_login_with_stored_credential_does_not_hang() {
        let (mut mode, terminal, _session, _tmp) = mode_harness().await;
        mode.init().await;

        // First login stores the credential (full flow incl. refresh).
        mode.ui_state.handle_login_command(Some("minimax-cn")).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        mode.ui_state.ui.tick(std::time::Instant::now());
        terminal.feed("sk-minimax-test");
        terminal.feed("\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            mode.ui_state.ui.tick(std::time::Instant::now());
            if !selector_mounted(&mode.ui_state) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "first login stalled");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Second `/login` must reach the prompt again (no hang).
        mode.ui_state.handle_login_command(Some("minimax-cn")).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        mode.ui_state.ui.tick(std::time::Instant::now());
        let rendered = lock(&mode.ui_state.active_selector)
            .as_ref()
            .map(|entry| entry.lock().unwrap().render(60).join("\n"))
            .unwrap_or_default();
        assert!(
            rendered.contains("Enter MiniMax CN API key"),
            "second login prompt; rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn login_minimax_cn_shows_key_prompt_and_stores_credential() {
        let (mut mode, terminal, _session, _tmp) = mode_harness().await;
        mode.init().await;
        mode.ui_state.handle_login_command(Some("minimax-cn")).await;
        // The login task runs on the spawned async; let it reach the prompt.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        mode.ui_state.ui.tick(std::time::Instant::now());
        let rendered = lock(&mode.ui_state.active_selector)
            .as_ref()
            .map(|entry| entry.lock().unwrap().render(60).join("\n"))
            .unwrap_or_default();
        assert!(
            rendered.contains("Enter MiniMax CN API key"),
            "key prompt visible; rendered: {rendered}"
        );

        // Type the key and confirm; the credential lands in the store.
        terminal.feed("sk-minimax-test");
        terminal.feed("\r");
        mode.ui_state.ui.tick(std::time::Instant::now());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let credentials = mode
            .ui_state
            .session()
            .model_runtime()
            .list_credentials()
            .await
            .expect("list credentials");
        assert!(
            credentials
                .iter()
                .any(|entry| entry.provider_id == "minimax-cn"
                    && entry.credential_type == CredentialType::ApiKey),
            "stored credential; got: {:?}",
            credentials
                .iter()
                .map(|entry| (entry.provider_id.as_str(), entry.credential_type))
                .collect::<Vec<_>>()
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
    async fn handle_reload_command_mounts_reload_box() {
        let (mode, _terminal, _session, _tmp) = mode_harness().await;
        mode.ui_state.handle_reload_command();
        // The reload box replaces the editor content (interactive-mode.ts:
        // 6442-6462); the reload itself runs in the background.
        let mounted = lock(&mode.ui_state.editor_container).render(80).join("\n");
        assert!(
            mounted.contains("Reloading keybindings, extensions, skills, prompts,"),
            "reload box: {mounted}"
        );
    }

    /// #62: the `/reload` consumer surfaces the built-in command conflict
    /// diagnostics from the live extension runner
    /// (interactive-mode.ts:710-723).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reload_warns_about_builtin_command_conflicts() {
        use rpi_ext_host::api::ExtensionApi;
        use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};

        let factory: ExtensionFactory = Arc::new(|api: ExtensionApi| {
            api.register_command(
                "model",
                Some("shadow".to_owned()),
                Arc::new(|_args, _ctx| Box::pin(async { Ok(()) })),
            )
            .expect("register command");
            Box::pin(async { Ok(()) })
        });
        let host = Arc::new(rpi_ext_host::host::NativeExtensionHost::new("/w3-cwd"));
        let errors = host
            .load_inline(&[InlineExtension::Anonymous(factory)])
            .await;
        assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

        let TestSession {
            _tmp,
            runtime,
            session,
            cwd: _,
        } = build_test_session_with(None, Some(host)).await;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        // The diagnostic producer sees the registered command.
        let commands = session.extension_runner().registered_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "model");

        mode.ui_state.handle_reload_command();
        let chat = rendered_chat(&mode.ui_state);
        // The message wraps at the render width; normalize whitespace so the
        // full warning text is matched.
        let normalized = chat.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains(
                "Extension command '/model' conflicts with built-in interactive command. Skipping in autocomplete."
            ),
            "conflict warning: {chat}"
        );
        // Let the spawned reload body settle before the temp dir drops.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // ---------------------------------------------------------------------
    // /import confirmation + missing-cwd recovery (V17-08 #63)
    // ---------------------------------------------------------------------

    /// Build an outside JSONL whose header cwd is `cwd` (an argument, so
    /// callers can point at a missing directory).
    fn write_import_file(path: &std::path::Path, cwd: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("import parent dir");
        }
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "imported",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd.to_string_lossy(),
        });
        std::fs::write(path, format!("{header}\n")).expect("write import file");
    }

    /// #63 /import: the confirmation prompt gates the import; declining
    /// keeps the current session untouched.
    #[tokio::test]
    async fn import_command_decline_keeps_current_session() {
        let (mut mode, terminal, _session, tmp) = switchable_harness_with_session_dir().await;
        mode.init().await;
        let source = tmp.path().join("outside").join("imported.jsonl");
        write_import_file(&source, &tmp.path().join("cwd"));
        let session_id_before = mode.ui_state.session().session_id();

        // Decline: down to "No", Enter.
        let ui = Arc::clone(&mode.ui_state);
        let feeder = async move {
            answer_mounted_dialog(&ui, &terminal, "Import session", &["\x1b[B", "\r"]).await;
        };
        let command = format!("/import {}", source.display());
        let (_, ()) = tokio::join!(mode.handle_import_command(&command), feeder);

        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Import cancelled"), "chat: {chat}");
        assert_eq!(
            mode.ui_state.session().session_id(),
            session_id_before,
            "declining must not replace the session"
        );
    }

    /// #63 /import: `MissingSessionCwdError` routes to the cwd prompt and the
    /// confirmed fallback cwd retries the import successfully (instead of the
    /// fatal path).
    #[tokio::test]
    async fn import_command_missing_cwd_prompts_and_retries() {
        let (mut mode, terminal, _session, tmp) = switchable_harness_with_session_dir().await;
        mode.init().await;
        let source = tmp.path().join("outside").join("imported.jsonl");
        let missing_cwd = tmp.path().join("gone-cwd");
        write_import_file(&source, &missing_cwd);
        let expected_cwd = tmp.path().join("cwd");

        let ui = Arc::clone(&mode.ui_state);
        let feeder = async move {
            // Confirm the import, then confirm the cwd prompt (fallback cwd).
            answer_mounted_dialog(&ui, &terminal, "Import session", &["\r"]).await;
            answer_mounted_dialog(&ui, &terminal, "Session cwd not found", &["\r"]).await;
        };
        let command = format!("/import {}", source.display());
        let (_, ()) = tokio::join!(mode.handle_import_command(&command), feeder);

        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Session imported from:"), "chat: {chat}");
        let imported_cwd = lock(&mode.ui_state.session().session_manager())
            .get_cwd()
            .to_path_buf();
        assert_eq!(
            imported_cwd, expected_cwd,
            "imported under the fallback cwd"
        );
    }

    /// A provider whose catalog refresh never completes — the `/model`
    /// command probe's 15 s bound must cut it (V17-08 #63).
    struct NeverFinishingCatalogProvider {
        id: String,
        auth: rpi_ai::auth::ProviderAuth,
        model: Model,
    }

    impl rpi_ai::models::Provider for NeverFinishingCatalogProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn name(&self) -> &str {
            &self.id
        }

        fn base_url(&self) -> Option<&str> {
            None
        }

        fn headers(&self) -> Option<&rpi_ai::types::ProviderHeaders> {
            None
        }

        fn auth(&self) -> &rpi_ai::auth::ProviderAuth {
            &self.auth
        }

        fn get_models(&self) -> Vec<Model> {
            vec![self.model.clone()]
        }

        fn stream(
            &self,
            _model: &Model,
            _context: &rpi_ai::types::TranscriptContext,
            _options: Option<rpi_ai::types::StreamOptions>,
        ) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
            rpi_ai::utils::event_stream::AssistantMessageEventStream::new()
        }

        fn stream_simple(
            &self,
            _model: &Model,
            _context: &rpi_ai::types::TranscriptContext,
            _options: Option<rpi_ai::types::SimpleStreamOptions>,
        ) -> Result<rpi_ai::utils::event_stream::AssistantMessageEventStream, String> {
            Err("NeverFinishingCatalogProvider does not stream".to_owned())
        }

        fn refresh_models(
            &self,
            _context: rpi_ai::models::RefreshModelsContext,
        ) -> Option<futures::future::BoxFuture<'_, Result<(), rpi_ai::auth::ModelsError>>> {
            Some(Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(600)).await;
                Ok(())
            }))
        }
    }

    /// #63 /model: the command probe (second `refreshModelCatalogs`
    /// consumption point) passes a 15 s waiter bound — a hanging catalog
    /// refresh times out with the upstream warning and the selector still
    /// opens over the cached models.
    #[tokio::test(start_paused = true)]
    async fn model_command_probe_times_out_and_warns() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let model = session.model().expect("session model");
        session
            .model_runtime()
            .register_native_provider(Arc::new(NeverFinishingCatalogProvider {
                id: "never-catalog".to_owned(),
                auth: rpi_ai::auth::ProviderAuth {
                    api_key: None,
                    oauth: None,
                },
                model,
            }))
            .await
            .expect("register provider");

        mode.ui_state.handle_model_command("does/not-exist").await;

        let chat = rendered_chat(&mode.ui_state);
        assert!(
            chat.contains("Model refresh timed out; searching cached models."),
            "chat: {chat}"
        );
        assert!(
            selector_mounted(&mode.ui_state),
            "the selector still opens with cached models"
        );
    }

    /// #63 /import: declining the missing-cwd prompt cancels the import and
    /// keeps the current session.
    #[tokio::test]
    async fn import_command_missing_cwd_cancel_keeps_session() {
        let (mut mode, terminal, _session, tmp) = switchable_harness_with_session_dir().await;
        mode.init().await;
        let source = tmp.path().join("outside").join("imported.jsonl");
        write_import_file(&source, &tmp.path().join("gone-cwd"));
        let session_id_before = mode.ui_state.session().session_id();

        // Confirm the import, decline the cwd prompt (down + Enter).
        let ui = Arc::clone(&mode.ui_state);
        let feeder = async move {
            answer_mounted_dialog(&ui, &terminal, "Import session", &["\r"]).await;
            answer_mounted_dialog(&ui, &terminal, "Session cwd not found", &["\x1b[B", "\r"]).await;
        };
        let command = format!("/import {}", source.display());
        let (_, ()) = tokio::join!(mode.handle_import_command(&command), feeder);

        let chat = rendered_chat(&mode.ui_state);
        assert!(chat.contains("Import cancelled"), "chat: {chat}");
        assert_eq!(
            mode.ui_state.session().session_id(),
            session_id_before,
            "cancelling the cwd prompt must not replace the session"
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

    /// B1 (interactive-mode.ts:4947-4949): clone failure is a recoverable
    /// `showError`, not a fatal exit. If the handler took the fatal path it
    /// would `process::exit` and kill this test binary, so completing the
    /// assertions below proves the recoverable path was taken.
    #[tokio::test]
    async fn handle_clone_command_failure_shows_error_not_fatal() {
        let (mut mode, _terminal, session, _tmp) =
            failing_factory_harness("clone factory exploded").await;
        // Give the session a leaf to fork from.
        lock(&session.session_manager())
            .append_message(user_message("fork me"))
            .expect("append message");
        mode.handle_clone_command().await;
        let chat = rendered_chat(&mode.ui_state);
        assert!(
            chat.contains("Error: clone factory exploded"),
            "clone failure surfaces a recoverable error, chat: {chat}"
        );
        assert!(
            !chat.contains("Cloned to new session"),
            "clone did not succeed, chat: {chat}"
        );
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
        // message (session-manager.ts:1172-1194), so a user-message-only
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
            1,
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
                api: rpi_ai::types::ApiKind("anthropic-messages".into()),
                provider: "anthropic".into(),
                base_url: String::new(),
                reasoning: false,
                thinking_level_map: None,
                input: Vec::new(),
                input_limits: None,
                cost: rpi_ai::types::ModelCost::default(),
                prompt_cache: None,
                context_window: 200_000,
                max_tokens: 64_000,
                headers: None,
                compat: None,
                sampling_params: None,
                sampling_params_by_thinking_level: None,
            },
            Model {
                id: "m1".into(),
                name: "M1".into(),
                api: rpi_ai::types::ApiKind("openai-completions".into()),
                provider: "custom".into(),
                base_url: String::new(),
                reasoning: false,
                thinking_level_map: None,
                input: Vec::new(),
                input_limits: None,
                cost: rpi_ai::types::ModelCost::default(),
                prompt_cache: None,
                context_window: 200_000,
                max_tokens: 64_000,
                headers: None,
                compat: None,
                sampling_params: None,
                sampling_params_by_thinking_level: None,
            },
            Model {
                id: "m1".into(),
                name: "M1 alt".into(),
                api: rpi_ai::types::ApiKind("openai-completions".into()),
                provider: "other".into(),
                base_url: String::new(),
                reasoning: false,
                thinking_level_map: None,
                input: Vec::new(),
                input_limits: None,
                cost: rpi_ai::types::ModelCost::default(),
                prompt_cache: None,
                context_window: 200_000,
                max_tokens: 64_000,
                headers: None,
                compat: None,
                sampling_params: None,
                sampling_params_by_thinking_level: None,
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
                Some(value) => rpi_test_env::set_var("PATH", value),
                None => rpi_test_env::remove_var("PATH"),
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
        rpi_test_env::set_var("PATH", &path);
        PathEnvRestore { previous }
    }

    /// Environment keys the paste chain reads. Tests clear all of them so
    /// the ambient session (Wayland / X11 / WSL / Termux) cannot pick a
    /// branch, then set only the branch they exercise.
    const PASTE_ENV_KEYS: [&str; 6] = [
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "DISPLAY",
        "WSL_DISTRO_NAME",
        "WSLENV",
        "TERMUX_VERSION",
    ];

    struct PasteEnvRestore {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl Drop for PasteEnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.previous {
                match value {
                    Some(value) => rpi_test_env::set_var(key, value),
                    None => rpi_test_env::remove_var(key),
                }
            }
        }
    }

    /// Clears every environment key the paste chain reads (restores on
    /// drop); callers set the branch they exercise explicitly.
    fn clear_paste_env() -> PasteEnvRestore {
        let previous = PASTE_ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in PASTE_ENV_KEYS {
            rpi_test_env::remove_var(key);
        }
        PasteEnvRestore { previous }
    }

    /// Write fake `xclip`/`wl-paste` executables: `-t TARGETS` advertises
    /// `image/png` only when `image` is true (otherwise `text/plain`), the
    /// `-t image/png` probe emits PNG bytes when `image` is true, and plain
    /// text reads always print `pasted-text`. Every call is appended to
    /// `arguments_log` when given, so tests can assert which targets were
    /// probed.
    fn fake_clipboard_tools_with_log(
        dir: &TempDir,
        image: bool,
        arguments_log: Option<&std::path::Path>,
    ) -> std::path::PathBuf {
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let image_payload = if image {
            "printf '\\211PNG\\r\\n\\032\\n';\n"
        } else {
            ""
        };
        let targets_payload = if image {
            "printf 'image/png\\n';\n"
        } else {
            "printf 'text/plain\\n';\n"
        };
        let log_line = match arguments_log {
            Some(log) => format!("printf '%s\\n' \"$*\" >> '{}';\n", log.display()),
            None => String::new(),
        };
        for name in ["xclip", "wl-paste"] {
            let script = bin.join(name);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\n{log_line}for arg in \"$@\"; do\n  if [ \"$arg\" = TARGETS ]; then\n    {targets_payload}    exit 0\n  fi\n  if [ \"$arg\" = --list-types ]; then\n    {targets_payload}    exit 0\n  fi\n  if [ \"$arg\" = image/png ]; then\n    {image_payload}exit 0\n  fi\ndone\nprintf pasted-text\n"
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

    fn fake_clipboard_tools(dir: &TempDir, image: bool) -> std::path::PathBuf {
        fake_clipboard_tools_with_log(dir, image, None)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn background_paste_reads_off_thread_and_applies_on_drain() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = fake_clipboard_tools(&tmp, false);
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.start_clipboard_paste();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            ui.drain_events();
            if !lock(&ui.editor).get_text().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock(&ui.editor).get_text(), "pasted-text");
        assert!(
            !ui.clipboard_paste_in_flight.load(Ordering::SeqCst),
            "the in-flight flag must be released after the read"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_with_clipboard_image_inserts_temp_path() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = fake_clipboard_tools(&tmp, true);
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();

        // The PNG bytes are written to `rpi-clipboard-{uuid}.png` under the
        // temp dir and the path is inserted at the cursor
        // (interactive-mode.ts:2633-2640).
        let inserted = lock(&ui.editor).get_text();
        let temp_prefix = std::env::temp_dir().display().to_string();
        assert!(
            inserted.starts_with(&temp_prefix),
            "inserted absolute temp path: {inserted}"
        );
        assert!(
            inserted.contains("rpi-clipboard-") && inserted.ends_with(".png"),
            "uuid file name: {inserted}"
        );
        let bytes = std::fs::read(&inserted).expect("clipboard image file must exist");
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&inserted)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "clipboard images must not be world-readable");
        }
        let _ = std::fs::remove_file(&inserted);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_without_image_inserts_clipboard_text() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
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
    async fn paste_does_not_probe_unadvertised_image_targets() {
        // Regression for #9786 (`8676a0dcd`).
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let log = tmp.path().join("clipboard-args.log");
        let bin = fake_clipboard_tools_with_log(&tmp, false, Some(&log));
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();

        // The text fallback still runs, but the image probe was skipped.
        assert_eq!(lock(&ui.editor).get_text(), "pasted-text");
        let log = std::fs::read_to_string(&log).expect("argument log");
        assert!(log.contains("TARGETS"), "targets are listed first: {log}");
        assert!(
            !log.contains("image/png"),
            "unadvertised image targets must not be probed: {log}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_does_not_probe_image_targets_when_listing_fails() {
        // Regression for #9786 (`8676a0dcd`).
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let log = tmp.path().join("clipboard-args.log");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("xclip");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}';\nfor arg in \"$@\"; do\n  if [ \"$arg\" = TARGETS ]; then exit 1; fi\n  if [ \"$arg\" = image/png ]; then printf 'PNG'; exit 0; fi\ndone\nprintf pasted-text\n",
                log.display()
            ),
        )
        .expect("write fake xclip");
        let wl_paste = bin.join("wl-paste");
        std::fs::write(&wl_paste, "#!/bin/sh\nprintf pasted-text\n").expect("write fake wl-paste");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for script in [&script, &wl_paste] {
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake clipboard tool");
            }
        }
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();

        assert_eq!(lock(&ui.editor).get_text(), "pasted-text");
        let log = std::fs::read_to_string(&log).expect("argument log");
        assert!(log.contains("TARGETS"), "targets are listed first: {log}");
        assert!(
            !log.contains("image/png"),
            "a failed listing must skip the image probe: {log}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_without_clipboard_tools_shows_status() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        // The host really has a working clipboard tool (dev machine): the
        // status path is environment-dependent, so skip instead of flaking.
        // CI has none, which is what this test asserts.
        if read_clipboard_image().is_some() || read_clipboard_text().is_some() {
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
    // V17-10 #66: the clipboard paste chain (files → images → text).
    // ---------------------------------------------------------------------

    /// Write a fake clipboard tool answering a MIME listing (`TARGETS` /
    /// `--list-types`) with `listing` (shell `\n` escapes are interpreted)
    /// and every other invocation with the bytes of `payload` (or
    /// `pasted-text`); every call is appended to `log` when given.
    fn fake_reader_tool(
        dir: &TempDir,
        name: &str,
        listing: &str,
        payload: Option<&[u8]>,
        log: Option<&std::path::Path>,
    ) -> std::path::PathBuf {
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join(name);
        let payload_command = match payload {
            Some(bytes) => {
                let payload_path = bin.join(format!("{name}-payload.bin"));
                std::fs::write(&payload_path, bytes).expect("write fake payload");
                format!("cat '{}'", payload_path.display())
            }
            None => "printf pasted-text".to_string(),
        };
        let log_line = match log {
            Some(log) => format!("printf '%s\\n' \"$*\" >> '{}';\n", log.display()),
            None => String::new(),
        };
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{log_line}for arg in \"$@\"; do\n  if [ \"$arg\" = TARGETS ] || [ \"$arg\" = --list-types ]; then\n    printf '{listing}\\n'; exit 0\n  fi\ndone\n{payload_command}\n"
            ),
        )
        .expect("write fake tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake tool");
        }
        script
    }

    #[test]
    fn cursor_spacing_matches_upstream_rules() {
        // interactive-mode.ts:3160-3165.
        assert_eq!(apply_cursor_spacing("/p", "abc", 3), " /p");
        assert_eq!(apply_cursor_spacing("/p", "abc", 0), "/p ");
        assert_eq!(apply_cursor_spacing("/p", "abcd", 2), " /p ");
        assert_eq!(apply_cursor_spacing("/p", "ab ", 3), "/p");
        assert_eq!(apply_cursor_spacing("/p", " ab", 1), "/p ");
        assert_eq!(apply_cursor_spacing("/p", "", 0), "/p");
    }

    #[test]
    fn image_mime_helpers_match_upstream() {
        // clipboard-image.ts:26-65.
        assert_eq!(base_mime_type("Image/PNG; charset=x"), "image/png");
        assert_eq!(extension_for_image_mime_type("image/png"), Some("png"));
        assert_eq!(extension_for_image_mime_type("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for_image_mime_type("image/webp"), Some("webp"));
        assert_eq!(extension_for_image_mime_type("image/gif"), Some("gif"));
        assert_eq!(extension_for_image_mime_type("image/bmp"), None);
        assert_eq!(
            select_preferred_image_mime_type("text/plain\nimage/gif\nimage/png\n").as_deref(),
            Some("image/png"),
            "fixed preference order: png before gif"
        );
        assert_eq!(
            select_preferred_image_mime_type("image/bmp\n").as_deref(),
            Some("image/bmp"),
            "any image/* is selected when no supported type is on offer"
        );
        assert_eq!(select_preferred_image_mime_type("text/plain\n"), None);
    }

    #[test]
    fn file_uri_list_parsing_matches_rfc_2483() {
        assert_eq!(
            parse_file_uri_list(
                b"# comment\nfile:///tmp/a%20b.txt\nfile://localhost/tmp/d.txt\nfile://otherhost/tmp/c.txt\nhttps://x/y\n\n",
                "text/uri-list"
            ),
            Some(vec![
                "/tmp/a b.txt".to_string(),
                "/tmp/d.txt".to_string()
            ]),
            "comments, non-file URLs and non-local hosts are skipped"
        );
        assert_eq!(parse_file_uri_list(b"", "text/uri-list"), None);
        // GNOME's flavour carries a copy/cut header line.
        assert_eq!(
            parse_file_uri_list(b"copy\nfile:///tmp/x\n", "x-special/gnome-copied-files"),
            Some(vec!["/tmp/x".to_string()])
        );
        assert_eq!(
            select_file_list_mime_type("image/png\ntext/uri-list\n"),
            Some("text/uri-list")
        );
        assert_eq!(select_file_list_mime_type("text/plain\n"), None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_file_paths_inserts_joined_paths() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        fake_reader_tool(
            &tmp,
            "xclip",
            "text/uri-list",
            Some(b"file:///tmp/a%20b.txt\nfile:///tmp/c.txt\n"),
            None,
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "/tmp/a b.txt\n/tmp/c.txt");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_file_paths_quote_in_bash_mode_and_space_at_cursor() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        fake_reader_tool(
            &tmp,
            "xclip",
            "text/uri-list",
            Some(b"file:///tmp/a%20b.txt\nfile:///tmp/c.txt\n"),
            None,
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        // Bash mode: `quoteIfNeeded` joined by a single space
        // (interactive-mode.ts:3159).
        *lock(&ui.is_bash_mode) = true;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "'/tmp/a b.txt' /tmp/c.txt");

        // Non-bash: joined by newlines; the cursor after "abc" adds the
        // leading spacing space (interactive-mode.ts:3160-3165).
        lock(&ui.editor).set_text("abc");
        *lock(&ui.is_bash_mode) = false;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "abc /tmp/a b.txt\n/tmp/c.txt");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_file_paths_reject_control_characters() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        fake_reader_tool(
            &tmp,
            "xclip",
            "text/uri-list",
            Some(b"file:///tmp/a%01b.txt\n"),
            None,
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "", "nothing inserted");
        let rendered = lock(&ui.chat_container).render(120).join("\n");
        assert!(
            rendered.contains("Clipboard file path contains control characters"),
            "control-character guard surfaces the upstream message: {rendered}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_file_paths_short_circuit_image_and_text() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let log = tmp.path().join("args.log");
        fake_reader_tool(
            &tmp,
            "xclip",
            "text/uri-list\\nimage/png",
            Some(b"file:///tmp/f\n"),
            Some(&log),
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "/tmp/f");
        let log = std::fs::read_to_string(&log).expect("argument log");
        assert!(log.contains("-t text/uri-list -o"), "file list read: {log}");
        assert!(
            !log.contains("image/png"),
            "the image stage must not run once file paths were found: {log}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_wayland_file_paths_via_wl_paste() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        rpi_test_env::set_var("WAYLAND_DISPLAY", "wayland-0");
        let tmp = TempDir::new();
        fake_reader_tool(
            &tmp,
            "wl-paste",
            "text/uri-list",
            Some(b"file:///tmp/wayland%20file.txt\n"),
            None,
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "/tmp/wayland file.txt");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_wayland_empty_clipboard_does_not_fall_through_to_x11() {
        // clipboard-image.ts:89-90: an empty Wayland clipboard must not
        // fall through to stale X11 contents.
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        rpi_test_env::set_var("WAYLAND_DISPLAY", "wayland-0");
        let tmp = TempDir::new();
        let log = tmp.path().join("xclip.log");
        fake_reader_tool(&tmp, "wl-paste", "text/plain", None, None);
        fake_reader_tool(
            &tmp,
            "xclip",
            "image/png",
            Some(b"\x89PNG\r\n\x1a\n"),
            Some(&log),
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        assert_eq!(lock(&ui.editor).get_text(), "pasted-text");
        assert!(
            !log.exists(),
            "the xclip fallback must be skipped when wl-paste reported no image"
        );
    }

    /// The image hit must short-circuit the text fallback (task §4 chain
    /// order: files → image → text).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_image_short_circuits_the_text_fallback() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let log = tmp.path().join("args.log");
        fake_reader_tool(
            &tmp,
            "xclip",
            "image/png",
            Some(b"\x89PNG\r\n\x1a\n"),
            Some(&log),
        );
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        let inserted = lock(&ui.editor).get_text();
        assert!(
            inserted.ends_with(".png"),
            "image path inserted: {inserted}"
        );
        let log = std::fs::read_to_string(&log).expect("argument log");
        assert!(
            !log.lines().any(|line| line == "-selection clipboard -o"),
            "the text fallback must not be read after an image hit: {log}"
        );
        let _ = std::fs::remove_file(&inserted);
    }

    /// Upstream `maxBufferBytes` default (`options?.maxBufferBytes ?? 50 *
    /// 1024 * 1024`, clipboard-command.ts:38): past the cap the read
    /// aborts and resolves `undefined` (rpi `None`).
    #[cfg(unix)]
    #[test]
    fn run_clipboard_command_aborts_past_the_buffer_cap() {
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("huge");
        std::fs::write(
            &script,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 100 ]; do printf '0123456789012345678901234567890123456789'; i=$((i+1)); done\n",
        )
        .expect("write fake tool");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake tool");
        }
        let small = run_clipboard_command_with_cap(
            script.to_str().expect("script path"),
            &[],
            Duration::from_millis(5000),
            32,
        );
        assert!(small.is_none(), "output past the cap yields None");
        let within = run_clipboard_command_with_cap(
            script.to_str().expect("script path"),
            &[],
            Duration::from_millis(5000),
            8192,
        );
        assert!(
            within.is_some_and(|bytes| bytes.len() == 4000),
            "a payload within the cap is returned whole"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_image_uses_the_mime_derived_extension() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        fake_reader_tool(&tmp, "xclip", "image/jpeg", Some(b"JPEGDATA"), None);
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        let inserted = lock(&ui.editor).get_text();
        assert!(inserted.ends_with(".jpg"), "jpeg maps to .jpg: {inserted}");
        assert_eq!(
            std::fs::read(&inserted).expect("image written"),
            b"JPEGDATA"
        );
        let _ = std::fs::remove_file(&inserted);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_unsupported_image_mime_converts_to_png() {
        // `convertToPng` (clipboard-image.ts:71-87): a BMP offer is
        // converted, so the file lands as `.png` (not a BMP named .png).
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bmp = {
            let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut out, image::ImageFormat::Bmp)
                .expect("encode bmp");
            out.into_inner()
        };
        fake_reader_tool(&tmp, "xclip", "image/bmp", Some(&bmp), None);
        let _path_restore = prepend_path(&tmp.path().join("bin"));

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        let inserted = lock(&ui.editor).get_text();
        assert!(inserted.ends_with(".png"), "converted to PNG: {inserted}");
        let bytes = std::fs::read(&inserted).expect("image written");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(image::load_from_memory(&bytes).is_ok(), "valid PNG bytes");
        let _ = std::fs::remove_file(&inserted);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn paste_wsl_falls_back_to_powershell() {
        // clipboard-image.ts:113-170/219-220: Win+Shift+S images land on
        // the Windows clipboard, reached through PowerShell on WSL.
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        rpi_test_env::set_var("WSL_DISTRO_NAME", "Ubuntu");
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        for name in ["xclip", "wl-paste"] {
            let script = bin.join(name);
            std::fs::write(&script, "#!/bin/sh\nexit 1\n").expect("write failing tool");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        let png = b"\x89PNG\r\n\x1a\nWSL";
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let powershell = bin.join("powershell.exe");
        std::fs::write(&powershell, format!("#!/bin/sh\nprintf '%s' '{encoded}'\n"))
            .expect("write fake powershell");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&powershell, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);

        let (mode, _terminal, _session, _tmp_keep) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_paste_image();
        let inserted = lock(&ui.editor).get_text();
        assert!(inserted.ends_with(".png"), "{inserted}");
        assert_eq!(std::fs::read(&inserted).expect("image written"), png);
        let _ = std::fs::remove_file(&inserted);
    }

    /// The macOS file-path arm parses the `POSIX path of` output of the
    /// `getFilePaths()` command-line equivalent (stubbed `osascript`).
    #[test]
    fn macos_file_paths_reader_parses_osascript_output() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("osascript");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '/tmp/a b.txt\\n/tmp/c.txt\\n'\n",
        )
        .expect("write fake osascript");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(
            read_clipboard_file_paths_macos(),
            Some(vec!["/tmp/a b.txt".to_string(), "/tmp/c.txt".to_string()])
        );
    }

    /// The Windows file-path arm parses the `FileDropList` output.
    #[test]
    fn windows_file_paths_reader_parses_powershell_output() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("powershell.exe");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' 'C:\\Users\\me\\a.txt' 'C:\\b.txt'\n",
        )
        .expect("write fake powershell");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(
            read_clipboard_file_paths_windows(),
            Some(vec![
                "C:\\Users\\me\\a.txt".to_string(),
                "C:\\b.txt".to_string()
            ])
        );
        assert!(
            WINDOWS_CLIPBOARD_FILE_PATHS_SCRIPT.contains("$_.FullName"),
            "the PowerShell projection must emit full paths, not FileInfo objects"
        );
        assert!(WINDOWS_CLIPBOARD_TEXT_SCRIPT.contains("Get-Clipboard -Raw"));
    }

    #[test]
    fn windows_file_paths_reader_rejects_powershell_table_output() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("powershell.exe");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' 'Mode LastWriteTime Length Name' '---- ------------- ------ ----' '-a--- 2026-01-01 0 a.txt'\n",
        )
        .expect("write fake powershell");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(read_clipboard_file_paths_windows(), None);
    }

    #[test]
    fn windows_text_reader_reads_raw_multiline_output() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("powershell.exe");
        std::fs::write(&script, "#!/bin/sh\nprintf 'line one\\nline two'\n")
            .expect("write fake powershell");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(
            read_clipboard_text_windows(),
            Some("line one\nline two".to_string())
        );
    }

    #[test]
    fn linux_text_reader_falls_back_to_xsel() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let xclip = bin.join("xclip");
        std::fs::write(&xclip, "#!/bin/sh\nexit 1\n").expect("write fake xclip");
        let xsel = bin.join("xsel");
        std::fs::write(&xsel, "#!/bin/sh\nprintf 'xsel text'\n").expect("write fake xsel");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for script in [&xclip, &xsel] {
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(read_clipboard_text_linux(), Some("xsel text".to_string()));
    }

    #[test]
    fn linux_text_reader_continues_after_an_empty_wayland_read() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        rpi_test_env::set_var("WAYLAND_DISPLAY", "wayland-0");
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let wl = bin.join("wl-paste");
        std::fs::write(&wl, "#!/bin/sh\n: > /dev/null\n").expect("write fake wl-paste");
        let xclip = bin.join("xclip");
        std::fs::write(&xclip, "#!/bin/sh\nprintf 'from xclip'\n").expect("write fake xclip");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for script in [&wl, &xclip] {
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(read_clipboard_text_linux(), Some("from xclip".to_string()));
    }

    #[test]
    fn applescript_string_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string("a\"b\\c"), "a\\\"b\\\\c");
    }

    /// The macOS image arm writes the pasteboard image to a temp file and
    /// derives the MIME from the class token (stubbed `osascript`).
    #[test]
    fn osascript_image_reader_writes_and_reads_the_temp_file() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let payload = bin.join("payload.png");
        std::fs::write(&payload, b"\x89PNG\r\n\x1a\nMAC").expect("write payload");
        let script = bin.join("osascript");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\npath=$(printf '%s' \"$2\" | sed -n 's/.*set outPath to \"\\(.*\\)\".*/\\1/p' | head -1)\ncat '{}' > \"$path\"\nprintf 'png'\n",
                payload.display()
            ),
        )
        .expect("write fake osascript");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);

        let image = read_clipboard_image_via_osascript().expect("image read");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.bytes, b"\x89PNG\r\n\x1a\nMAC");
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .expect("temp dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rpi-clipboard-")
                    && entry.path().extension().is_some_and(|ext| ext == "img")
            })
            .collect();
        assert!(leftovers.is_empty(), "the temp file is removed");
    }

    #[test]
    fn osascript_image_reader_returns_none_without_an_image() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _paste_env = clear_paste_env();
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("osascript");
        std::fs::write(&script, "#!/bin/sh\nprintf ''\n").expect("write fake osascript");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let _path_restore = prepend_path(&bin);
        assert_eq!(
            read_clipboard_image_via_osascript().map(|i| i.mime_type),
            None
        );
    }

    // ---------------------------------------------------------------------
    // /settings on_change handlers (apply_settings_change)
    // ---------------------------------------------------------------------

    fn assistant_message(
        content: Vec<rpi_ai::types::AssistantContent>,
        stop_reason: rpi_ai::types::StopReason,
    ) -> AgentMessage {
        AgentMessage::Assistant(rpi_ai::types::AssistantMessage {
            role: rpi_ai::types::AssistantRole::Assistant,
            content,
            api: rpi_ai::types::ApiKind("openai-completions".into()),
            provider: "custom".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: rpi_ai::types::Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 1_700_000_000_000,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
            duration_ms: None,
        })
    }

    fn text_content(text: &str) -> rpi_ai::types::AssistantContent {
        rpi_ai::types::AssistantContent::Text(rpi_ai::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })
    }

    fn thinking_content(text: &str) -> rpi_ai::types::AssistantContent {
        rpi_ai::types::AssistantContent::Thinking(rpi_ai::types::ThinkingContent {
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
        assert_eq!(lock(&ui.theme).name.as_deref(), Some("system"));

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
        rpi_test_env::set_var("COLORFGBG", "15;0");

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
                Some(value) => rpi_test_env::set_var("COLORFGBG", value),
                None => rpi_test_env::remove_var("COLORFGBG"),
            }
        }
    }

    #[tokio::test]
    async fn settings_hide_thinking_block_change_updates_in_place() {
        // G2 (b07e17faa): the setting now updates assistant messages in
        // place (`updateThinkingBlockVisibility`) instead of rebuilding the
        // chat container — non-entry chat children (status lines) survive.
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
                    rpi_ai::types::StopReason::Stop,
                ))
                .expect("append assistant");
        }
        ui.render_initial_messages();
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("some reasoning"), "visible: {rendered}");
        // No-rebuild signal: a status line is a chat child but not a
        // session entry, so only the in-place update keeps it.
        ui.show_status("no-rebuild-sentinel");

        apply_settings_change(ui, SettingsChange::HideThinkingBlock(true));
        assert!(*lock(&ui.hide_thinking_block));
        assert!(session.settings_manager(|s| s.get_hide_thinking_block()));

        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(rendered.contains("no-rebuild-sentinel"), "chat NOT rebuilt");
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

    /// A mounted transcript block that records the image-surface walk calls
    /// (`child instanceof ToolExecutionComponent` equivalent, #59).
    struct ImageSurfaceProbe {
        show_images: Arc<Mutex<Vec<bool>>>,
        image_widths: Arc<Mutex<Vec<usize>>>,
    }

    impl Component for ImageSurfaceProbe {
        fn render(&self, _width: usize) -> Vec<String> {
            Vec::new()
        }

        fn set_show_images(&mut self, show: bool) {
            lock(&self.show_images).push(show);
        }

        fn set_image_width_cells(&mut self, width: usize) {
            lock(&self.image_widths).push(width);
        }
    }

    #[tokio::test]
    async fn settings_show_images_and_image_width_update_mounted_children() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        let show_images = Arc::new(Mutex::new(Vec::new()));
        let image_widths = Arc::new(Mutex::new(Vec::new()));
        ui.add_chat_child(Box::new(ImageSurfaceProbe {
            show_images: Arc::clone(&show_images),
            image_widths: Arc::clone(&image_widths),
        }));

        let before = ui.show_images.load(Ordering::Relaxed);
        apply_settings_change(ui, SettingsChange::ShowImages(!before));
        assert_eq!(
            session.settings_manager(|s| s.get_show_images()),
            !before,
            "setting persisted"
        );
        assert_eq!(
            ui.show_images.load(Ordering::Relaxed),
            !before,
            "the mode's field is updated for components mounted later"
        );
        assert_eq!(
            lock(&show_images).as_slice(),
            &[!before],
            "mounted children got the new show-images value"
        );

        apply_settings_change(ui, SettingsChange::ImageWidthCells(120));
        assert_eq!(
            session.settings_manager(|s| s.get_image_width_cells()),
            120,
            "setting persisted"
        );
        assert_eq!(
            ui.image_width_cells.load(Ordering::Relaxed),
            120,
            "the mode's field is updated for components mounted later"
        );
        assert_eq!(
            lock(&image_widths).as_slice(),
            &[120],
            "mounted children got the new image width"
        );
    }

    #[tokio::test]
    async fn settings_transport_updates_the_live_agent() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        // The fixture's settings default to Auto; a plain settings write
        // (pre-#59) left the constructed agent's transport untouched.
        assert_eq!(session.agent().transport(), rpi_ai::types::Transport::Auto);

        apply_settings_change(
            ui,
            SettingsChange::Transport(rpi_ai::types::Transport::Websocket),
        );
        assert_eq!(
            session.settings_manager(|s| s.get_transport()),
            rpi_ai::types::Transport::Websocket,
            "setting persisted"
        );
        assert_eq!(
            session.agent().transport(),
            rpi_ai::types::Transport::Websocket,
            "the live agent reads the new transport on its next request"
        );
    }

    #[tokio::test]
    async fn settings_http_idle_timeout_reconfigures_the_dispatcher() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;

        apply_settings_change(ui, SettingsChange::HttpIdleTimeoutMs(60_000));
        assert_eq!(
            session
                .settings_manager(|s| s.get_http_idle_timeout_ms())
                .expect("valid setting"),
            60_000,
            "setting persisted"
        );
        assert_eq!(
            session.http_dispatcher().timeout_ms(),
            60_000,
            "the live dispatcher slot is reconfigured"
        );
    }

    #[tokio::test]
    async fn settings_enable_skill_commands_rebuilds_autocomplete() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        // Load a skill after the mode mounted (its init provider saw none).
        let skills_dir = lock(&session.resource_loader()).agent_dir().join("skills");
        let skill_file = skills_dir.join("test-skill").join("SKILL.md");
        std::fs::create_dir_all(skill_file.parent().expect("skill dir")).expect("create skill dir");
        std::fs::write(
            &skill_file,
            "---\nname: test-skill\ndescription: A test skill\n---\nbody\n",
        )
        .expect("write skill file");
        lock(&session.resource_loader()).reload();
        assert!(lock(&ui.skill_commands).is_empty(), "not yet rebuilt");

        // Toggling the setting rebuilds the provider (`setupAutocompleteProvider`,
        // interactive-mode.ts:4986-4990); the rebuild repopulates the map.
        apply_settings_change(ui, SettingsChange::EnableSkillCommands(true));
        assert!(
            lock(&ui.skill_commands).contains_key("skill:test-skill"),
            "provider rebuilt with the skill: {:?}",
            lock(&ui.skill_commands).keys().collect::<Vec<_>>()
        );

        apply_settings_change(ui, SettingsChange::EnableSkillCommands(false));
        assert!(
            lock(&ui.skill_commands).is_empty(),
            "provider rebuilt without skill commands"
        );
    }

    #[tokio::test]
    async fn settings_output_pad_updates_children_in_place_when_idle() {
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

        let before = lock(&ui.chat_container).render(60).join("\n");
        apply_settings_change(ui, SettingsChange::OutputPad(0));
        assert_eq!(session.settings_manager(|s| s.get_output_pad()), 0);
        assert_eq!(ui.output_pad.load(Ordering::Relaxed), 0);
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("rebuild-sentinel"),
            "no rebuild: the walk updates children in place (#10557)"
        );
        assert_ne!(before, rendered, "the walk reached the user message");
        assert!(
            !rendered.contains(" hello"),
            "user message repadded to outputPad 0: {rendered:?}"
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
            rpi_ai::types::StopReason::Pending,
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

    // V17-05 FR-A: OSC 7501 statuses reach the terminal through the drain
    // (program-status-reporter state authority, 02 §1.3 裁决 3).
    #[tokio::test]
    async fn program_status_reports_idle_working_done_and_idle_on_abort() {
        let (mode, terminal, _session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.report_program_status();
        let statuses = terminal.program_statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(
            statuses[0].state,
            rpi_tui::program_status::ProgramStatusState::Idle
        );
        assert_eq!(statuses[0].app.as_deref(), Some("rpi"));

        ui.push(UiCommand::AgentStart);
        ui.drain_events();
        // Dedup: a second report of the same state stays silent.
        ui.report_program_status();
        let statuses = terminal.program_statuses();
        assert_eq!(statuses.len(), 2);
        assert_eq!(
            statuses[1].state,
            rpi_tui::program_status::ProgramStatusState::Working
        );

        ui.push(UiCommand::MessageEnd(assistant_message(
            vec![text_content("done")],
            rpi_ai::types::StopReason::Stop,
        )));
        ui.push(UiCommand::AgentSettled { aborted: false });
        ui.drain_events();
        let statuses = terminal.program_statuses();
        assert_eq!(
            statuses.last().unwrap().state,
            rpi_tui::program_status::ProgramStatusState::Done
        );

        ui.push(UiCommand::AgentStart);
        ui.push(UiCommand::AgentSettled { aborted: true });
        ui.drain_events();
        let statuses = terminal.program_statuses();
        assert_eq!(
            statuses.last().unwrap().state,
            rpi_tui::program_status::ProgramStatusState::Idle
        );
    }

    // V17-05 FR-A: blocked reports come from the extension dialog / login
    // seams (`programStatus.setBlocked`).
    #[tokio::test]
    async fn program_status_reports_blocked_dialogs() {
        let (mode, terminal, _session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.set_program_status_blocked(
            "extension-dialog",
            Some(crate::modes::interactive::program_status::BlockedStatus {
                kind: rpi_tui::program_status::ProgramStatusKind::Permission,
                message: "Allow bash?".into(),
            }),
        );
        let statuses = terminal.program_statuses();
        let last = statuses.last().unwrap();
        assert_eq!(
            last.state,
            rpi_tui::program_status::ProgramStatusState::Blocked
        );
        assert_eq!(
            last.kind,
            Some(rpi_tui::program_status::ProgramStatusKind::Permission)
        );
        assert_eq!(last.message.as_deref(), Some("Allow bash?"));

        // Most recent dialog wins; clearing returns to the underlying idle.
        ui.set_program_status_blocked(
            "login",
            Some(crate::modes::interactive::program_status::BlockedStatus {
                kind: rpi_tui::program_status::ProgramStatusKind::Auth,
                message: "Log in to Anthropic".into(),
            }),
        );
        ui.set_program_status_blocked("login", None);
        let statuses = terminal.program_statuses();
        assert_eq!(
            statuses[statuses.len() - 2].kind,
            Some(rpi_tui::program_status::ProgramStatusKind::Auth)
        );
        assert_eq!(
            statuses.last().unwrap().state,
            rpi_tui::program_status::ProgramStatusState::Blocked
        );
        assert_eq!(
            statuses.last().unwrap().kind,
            Some(rpi_tui::program_status::ProgramStatusKind::Permission)
        );
    }

    /// Regression: the tui-mode settings callback fires inside the settings
    /// selector's `handle_input` with the renderer's inner lock held
    /// (`TuiMainScreen::tick` → `lock_inner().handle_input(...)`). The switch
    /// must be deferred to the drain (`UiCommand::SwitchTuiMode`): the
    /// synchronous path calls `capture_render_state`/`stop`/`start`, which
    /// take that same non-reentrant mutex — the driver thread self-deadlocked
    /// (the "/settings tui-mode switch hang").
    #[tokio::test]
    async fn settings_tui_mode_change_is_deferred_to_the_drain() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        assert_eq!(ui.ui.mode(), rpi_tui::tui::TuiMode::Regular);

        apply_settings_change(
            ui,
            SettingsChange::TuiMode(rpi_tui::tui::TuiMode::Fullscreen),
        );
        // Queue-only: no synchronous renderer access, no persistence yet.
        assert_eq!(
            ui.ui.mode(),
            rpi_tui::tui::TuiMode::Regular,
            "the switch must not run inside the dispatch"
        );
        assert_eq!(
            session.settings_manager(|s| s.get_tui_mode()),
            rpi_tui::tui::TuiMode::Fullscreen,
            "not persisted before the switch succeeds (the settings default is fullscreen)"
        );

        // The drain performs the switch + persists + confirms.
        ui.drain_events();
        assert_eq!(ui.ui.mode(), rpi_tui::tui::TuiMode::Fullscreen);
        assert_eq!(
            session.settings_manager(|s| s.get_tui_mode()),
            rpi_tui::tui::TuiMode::Fullscreen
        );

        // And back to regular through the same deferred path.
        apply_settings_change(ui, SettingsChange::TuiMode(rpi_tui::tui::TuiMode::Regular));
        ui.drain_events();
        assert_eq!(ui.ui.mode(), rpi_tui::tui::TuiMode::Regular);
        assert_eq!(
            session.settings_manager(|s| s.get_tui_mode()),
            rpi_tui::tui::TuiMode::Regular
        );
    }

    /// The overlay-open rejection survives the deferral: no switch, no
    /// persistence (rollback path through the drain).
    #[tokio::test]
    async fn settings_tui_mode_change_rejection_still_not_persisted() {
        let (mode, _terminal, session, _tmp) = mode_harness().await;
        let ui = &mode.ui_state;
        let overlay = shared_component_from_boxed(Box::new(rpi_tui::tui::Container::new()));
        let _handle = ui.ui.show_overlay(overlay, None);
        assert!(ui.ui.has_overlay_entries());

        apply_settings_change(
            ui,
            SettingsChange::TuiMode(rpi_tui::tui::TuiMode::Fullscreen),
        );
        ui.drain_events();
        assert_eq!(
            ui.ui.mode(),
            rpi_tui::tui::TuiMode::Regular,
            "overlay rejection: mode unchanged"
        );
        assert_eq!(
            session.settings_manager(|s| s.get_tui_mode()),
            rpi_tui::tui::TuiMode::Fullscreen,
            "overlay rejection: not persisted (the settings default is fullscreen)"
        );
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
            .name("rpi-tui-driver-test".to_string())
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

    /// V14-13 FR-F R5（a1bc0ec79，#8203）：llama.cpp 登录引导双态文案。
    #[test]
    fn llama_cpp_post_login_guidance_two_states() {
        assert_eq!(
            llama_cpp_post_login_guidance("Logged in to llama.cpp", 0),
            "Logged in to llama.cpp. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
        );
        assert_eq!(
            llama_cpp_post_login_guidance("Logged in to llama.cpp", 2),
            "Logged in to llama.cpp. Use /model to select a loaded llama.cpp model, or /llama to manage models."
        );
    }

    // ------------------------------------------------------------------
    // V17-10 #65: the tmux keyboard check through the real spawn path.
    // These mutate `TMUX`/`$PATH`, so they share `PATH_ENV_LOCK` with the
    // clipboard tests (the process environment is global).
    // ------------------------------------------------------------------

    /// Temporarily sets `TMUX` (restores on drop). `TMUX` is only read by
    /// the tmux keyboard check.
    struct TmuxEnvRestore {
        previous: Option<std::ffi::OsString>,
    }

    impl Drop for TmuxEnvRestore {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => rpi_test_env::set_var("TMUX", value),
                None => rpi_test_env::remove_var("TMUX"),
            }
        }
    }

    fn set_tmux_env(value: &str) -> TmuxEnvRestore {
        let previous = std::env::var_os("TMUX");
        rpi_test_env::set_var("TMUX", value);
        TmuxEnvRestore { previous }
    }

    /// Write a fake `tmux` answering the two `show -gv` queries.
    fn fake_tmux(dir: &TempDir, keys: &str, format: &str) -> std::path::PathBuf {
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("tmux");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = extended-keys ]; then\n    printf '%s\\n' '{keys}'\n    exit 0\n  fi\n  if [ \"$arg\" = extended-keys-format ]; then\n    printf '%s\\n' '{format}'\n    exit 0\n  fi\ndone\nexit 1\n"
            ),
        )
        .expect("write fake tmux");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake tmux");
        }
        bin
    }

    fn run_tmux_check(timeout: Duration) -> Option<String> {
        crate::modes::interactive::interactive_mode::tmux_check::check_tmux_keyboard_setup(timeout)
    }

    /// Issue #65 test plan ①: `TMUX` set + a stubbed `tmux` on `$PATH`
    /// returning `off` → the verbatim warning; `on` + `csi-u` → nothing.
    #[test]
    fn tmux_keyboard_check_warns_through_the_real_spawn_path() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let bin = fake_tmux(&tmp, "off", "xterm");
        let _path_restore = prepend_path(&bin);
        let _tmux_restore = set_tmux_env("1");
        assert_eq!(
            run_tmux_check(Duration::from_millis(2000)).as_deref(),
            Some(
                crate::modes::interactive::interactive_mode::tmux_check::EXTENDED_KEYS_OFF_WARNING
            )
        );
    }

    #[test]
    fn tmux_keyboard_check_passes_through_the_real_spawn_path() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let bin = fake_tmux(&tmp, "on", "csi-u");
        let _path_restore = prepend_path(&bin);
        let _tmux_restore = set_tmux_env("1");
        assert_eq!(run_tmux_check(Duration::from_millis(2000)), None);
    }

    /// Issue #65 test plan ②: a hanging `tmux` is killed at the timeout
    /// with no warning and no error surface.
    #[test]
    fn tmux_keyboard_check_times_out_silently() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("tmux");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").expect("write fake tmux");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake tmux");
        }
        let _path_restore = prepend_path(&bin);
        let _tmux_restore = set_tmux_env("1");
        assert_eq!(run_tmux_check(Duration::from_millis(150)), None);
    }

    /// Issue #65 test plan ② (real spawn path): a non-zero exit is silent.
    #[test]
    fn tmux_keyboard_check_stays_silent_on_a_non_zero_exit() {
        let _env_guard = PATH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake bin dir");
        let script = bin.join("tmux");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").expect("write fake tmux");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake tmux");
        }
        let _path_restore = prepend_path(&bin);
        let _tmux_restore = set_tmux_env("1");
        assert_eq!(run_tmux_check(Duration::from_millis(2000)), None);
    }
}
