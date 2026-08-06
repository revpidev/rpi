//! Model selector — port of
//! `packages/coding-agent/src/modes/interactive/components/model-selector.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme, runtime and callbacks are injected explicitly; upstream
//!   reads the global `theme` (theme.ts:799-816) and the
//!   `settingsManager` constructor argument. The upstream
//!   `settingsManager.setDefaultModelAndProvider(model.provider, model.id)`
//!   (model-selector.ts:351-356) becomes the `save_default` callback — the
//!   integration layer owns the settings manager.
//! - The mutable list state (model-selector.ts:48-66) lives behind an
//!   `Arc<Mutex<..>>` shared with the background refresh task: the refresh
//!   completes on a spawned task (upstream mutates the component after
//!   `await modelRuntime.refresh(...)`), and Rust cannot borrow the
//!   component from a spawned future — same pattern as bash-execution's
//!   shared `Loader`.
//! - The 15 s AbortController timeout (model-selector.ts:162-192) is not
//!   enforced: `ModelRuntime::refresh` takes no signal (model_runtime.rs:
//!   1089). TODO(port): enforce a timeout once the runtime gains
//!   cancellation.
//! - Upstream reports per-catalog refresh errors from `result.errors`
//!   (model-selector.ts:175-178); the port reports
//!   `ModelRuntime::get_error()` instead (the local `refresh` returns no
//!   error map).
//! - `sortModels` uses plain `provider.cmp` where upstream uses
//!   `localeCompare` (model-selector.ts:208); identical for ASCII provider
//!   ids.
//! - The `searchInput.onSubmit` handler (model-selector.ts:112-117) is
//!   unreachable: the component intercepts `tui.select.confirm` before the
//!   input ever sees Enter, so the port leaves it unset (the confirm branch
//!   selects the same item).
//! - List rows are truncated to the render width with `...` (same idiom as
//!   config-selector.rs); upstream `Text` children wrap instead.

use std::sync::{Arc, Mutex};

use pir_ai::models::models_are_equal;
use pir_ai::types::{Model, ModelThinkingLevel};
use pir_tui::components::input::Input;
use pir_tui::components::text::Text;
use pir_tui::fuzzy::fuzzy_filter;
use pir_tui::keybindings::get_keybindings;
use pir_tui::tui::{Component, Focusable, Tui};
use pir_tui::utils::truncate_to_width;

use crate::core::model_runtime::ModelRuntime;
use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_hint;
use super::model_search::{get_model_selector_search_text, ModelSearchItem};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `ModelItem` (model-selector.ts:19-23).
#[derive(Clone)]
struct ModelItem {
    provider: String,
    id: String,
    model: Model,
}

/// `ScopedModelItem` (model-selector.ts:25-28).
#[derive(Clone)]
struct ScopedModelItem {
    model: Model,
    thinking_level: Option<ModelThinkingLevel>,
}

/// `ModelScope` (model-selector.ts:30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelScope {
    All,
    Scoped,
}

/// Mutable selector state, shared with the background refresh task
/// (model-selector.ts:48-66) — see the module docs on the split.
struct SelectorState {
    all_models: Vec<ModelItem>,
    scoped_models: Vec<ScopedModelItem>,
    scoped_model_items: Vec<ModelItem>,
    active_models: Vec<ModelItem>,
    filtered_models: Vec<ModelItem>,
    selected_index: usize,
    error_message: Option<String>,
    refresh_status_message: String,
    refresh_status_success: bool,
    scope: ModelScope,
    /// Mirror of the search input value, read by the refresh task when it
    /// re-filters (model-selector.ts:187 reads `searchInput.getValue()`).
    search_value: String,
    closed: bool,
}

impl Default for SelectorState {
    fn default() -> Self {
        Self {
            all_models: Vec::new(),
            scoped_models: Vec::new(),
            scoped_model_items: Vec::new(),
            active_models: Vec::new(),
            filtered_models: Vec::new(),
            selected_index: 0,
            error_message: None,
            refresh_status_message: String::new(),
            refresh_status_success: false,
            scope: ModelScope::All,
            search_value: String::new(),
            closed: false,
        }
    }
}

/// `sortModels` (model-selector.ts:200-211): current model first, then by
/// provider.
fn sort_models(mut models: Vec<ModelItem>, current_model: &Option<Model>) -> Vec<ModelItem> {
    models.sort_by(|a, b| {
        let a_is_current = models_are_equal(current_model.as_ref(), Some(&a.model));
        let b_is_current = models_are_equal(current_model.as_ref(), Some(&b.model));
        match (a_is_current, b_is_current) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.provider.cmp(&b.provider),
        }
    });
    models
}

/// `loadModelsFromSnapshot` (model-selector.ts:139-160).
fn load_models_from_snapshot(
    state: &mut SelectorState,
    runtime: &ModelRuntime,
    current_model: &Option<Model>,
) {
    state.all_models = sort_models(
        runtime
            .get_available_snapshot()
            .into_iter()
            .map(|model| ModelItem {
                provider: model.provider.clone(),
                id: model.id.clone(),
                model,
            })
            .collect(),
        current_model,
    );
    // Refresh scoped entries against the catalog; keep the original when the
    // model is gone (model-selector.ts:146-149).
    state.scoped_models = state
        .scoped_models
        .iter()
        .map(
            |scoped| match runtime.get_model(&scoped.model.provider, &scoped.model.id) {
                Some(model) => ScopedModelItem {
                    model,
                    thinking_level: scoped.thinking_level,
                },
                None => scoped.clone(),
            },
        )
        .collect();
    state.scoped_model_items = state
        .scoped_models
        .iter()
        .map(|scoped| ModelItem {
            provider: scoped.model.provider.clone(),
            id: scoped.model.id.clone(),
            model: scoped.model.clone(),
        })
        .collect();
    state.active_models = if state.scope == ModelScope::Scoped {
        state.scoped_model_items.clone()
    } else {
        state.all_models.clone()
    };
    state.filtered_models = state.active_models.clone();
    let current_index = state
        .filtered_models
        .iter()
        .position(|item| models_are_equal(current_model.as_ref(), Some(&item.model)));
    state.selected_index = match current_index {
        Some(index) => index,
        None => state
            .selected_index
            .min(state.filtered_models.len().saturating_sub(1)),
    };
}

/// `setScope` (model-selector.ts:223-233).
fn set_scope(state: &mut SelectorState, scope: ModelScope, current_model: &Option<Model>) {
    if state.scope == scope {
        return;
    }
    state.scope = scope;
    state.active_models = if scope == ModelScope::Scoped {
        state.scoped_model_items.clone()
    } else {
        state.all_models.clone()
    };
    let current_index = state
        .active_models
        .iter()
        .position(|item| models_are_equal(current_model.as_ref(), Some(&item.model)));
    state.selected_index = current_index.unwrap_or(0);
    let query = state.search_value.clone();
    filter_models(state, &query);
}

/// `filterModels` (model-selector.ts:235-243).
fn filter_models(state: &mut SelectorState, query: &str) {
    state.filtered_models = if query.is_empty() {
        state.active_models.clone()
    } else {
        fuzzy_filter(state.active_models.clone(), query, |item: &ModelItem| {
            get_model_selector_search_text(&ModelSearchItem {
                id: item.id.clone(),
                provider: item.provider.clone(),
                name: Some(item.model.name.clone()),
            })
        })
    };
    state.selected_index = state
        .selected_index
        .min(state.filtered_models.len().saturating_sub(1));
}

/// `getScopeText` (model-selector.ts:213-217).
fn get_scope_text(theme: &Theme, scope: ModelScope) -> String {
    let all_text = if scope == ModelScope::All {
        theme.fg("accent", "all")
    } else {
        theme.fg("muted", "all")
    };
    let scoped_text = if scope == ModelScope::Scoped {
        theme.fg("accent", "scoped")
    } else {
        theme.fg("muted", "scoped")
    };
    format!(
        "{}{}{}{}",
        theme.fg("muted", "Scope: "),
        all_text,
        theme.fg("muted", " | "),
        scoped_text
    )
}

/// `getScopeHintText` (model-selector.ts:219-221).
fn get_scope_hint_text(theme: &Theme) -> String {
    format!(
        "{}{}",
        key_hint(theme, "tui.input.tab", "scope"),
        theme.fg("muted", " (all/scoped)")
    )
}

/// Component that renders a model selector with search
/// (model-selector.ts:35-361).
pub struct ModelSelectorComponent {
    theme: Arc<Theme>,
    tui: Tui,
    search_input: Input,
    focused: bool,
    current_model: Option<Model>,
    model_runtime: Arc<ModelRuntime>,
    /// `settingsManager.setDefaultModelAndProvider` hook
    /// (model-selector.ts:354).
    save_default: Box<dyn FnMut(&Model) + Send>,
    on_select: Box<dyn FnMut(Model) + Send>,
    on_cancel: Box<dyn FnMut() + Send>,
    state: Arc<Mutex<SelectorState>>,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
}

impl ModelSelectorComponent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        current_model: Option<Model>,
        model_runtime: Arc<ModelRuntime>,
        scoped_models: Vec<(Model, Option<ModelThinkingLevel>)>,
        theme: Arc<Theme>,
        tui: Tui,
        save_default: Box<dyn FnMut(&Model) + Send>,
        on_select: Box<dyn FnMut(Model) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        initial_search_input: Option<String>,
    ) -> Self {
        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("border", text))
        };
        let scope = if scoped_models.is_empty() {
            ModelScope::All
        } else {
            ModelScope::Scoped
        };
        let mut state = SelectorState {
            scoped_models: scoped_models
                .into_iter()
                .map(|(model, thinking_level)| ScopedModelItem {
                    model,
                    thinking_level,
                })
                .collect(),
            scope,
            search_value: initial_search_input.clone().unwrap_or_default(),
            ..SelectorState::default()
        };
        // Render the current snapshot immediately, then refresh in the
        // background (model-selector.ts:131-137).
        load_models_from_snapshot(&mut state, &model_runtime, &current_model);
        let query = state.search_value.clone();
        filter_models(&mut state, &query);

        let mut search_input = Input::new();
        if let Some(initial) = &initial_search_input {
            search_input.set_value(initial);
        }

        let component = Self {
            theme,
            tui: tui.clone(),
            search_input,
            focused: false,
            current_model,
            model_runtime,
            save_default,
            on_select,
            on_cancel,
            state: Arc::new(Mutex::new(state)),
            top_border: DynamicBorder::new(border_color.clone()),
            bottom_border: DynamicBorder::new(border_color),
        };
        component.tui.request_render(false);
        component.start_refresh();
        component
    }

    /// `refreshModels` (model-selector.ts:162-192): background catalog
    /// refresh, started from the constructor. The spawned task updates the
    /// shared [`SelectorState`] and requests a re-render when done.
    fn start_refresh(&self) {
        let state = self.state.clone();
        let runtime = self.model_runtime.clone();
        let current_model = self.current_model.clone();
        let tui = self.tui.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    // TODO(port): upstream aborts the refresh after 15 s via
                    // AbortController (model-selector.ts:163-168); the local
                    // `ModelRuntime::refresh` takes no signal
                    // (model_runtime.rs:1089), so the timeout is not
                    // enforced here.
                    runtime.refresh().await;
                    let mut state = lock(&state);
                    if state.closed {
                        return;
                    }
                    state.refresh_status_message = String::new();
                    // Upstream inspects `result.errors` per catalog
                    // (model-selector.ts:175-178); the local refresh returns
                    // no error map, so `get_error` covers catalog errors.
                    state.error_message = runtime.get_error();
                    if state.error_message.is_none() {
                        state.refresh_status_message = "Model catalogs refreshed.".to_string();
                        state.refresh_status_success = true;
                    }
                    load_models_from_snapshot(&mut state, &runtime, &current_model);
                    let query = state.search_value.clone();
                    filter_models(&mut state, &query);
                    drop(state);
                    tui.request_render(false);
                });
            }
            Err(_) => {
                // No tokio runtime on this thread (unit tests outside
                // #[tokio::test]): the constructor already rendered the
                // snapshot synchronously.
            }
        }
    }

    /// `close` (model-selector.ts:194-198). Upstream also clears the refresh
    /// timeout and aborts the in-flight refresh; the port has neither (see
    /// module docs), so it only marks the component closed.
    fn close(&mut self) {
        lock(&self.state).closed = true;
    }

    /// `handleSelect` (model-selector.ts:351-356).
    fn handle_select(&mut self, model: Model) {
        self.close();
        (self.save_default)(&model);
        (self.on_select)(model);
    }

    /// `getSearchInput` (model-selector.ts:358-360).
    pub fn get_search_input(&self) -> &Input {
        &self.search_input
    }
}

impl Component for ModelSelectorComponent {
    fn handle_input(&mut self, data: &str) {
        // Keybinding matching runs inside a block so the read guard is
        // dropped before the fall-through forwards to the search input:
        // `Input::handle_input` takes a second read lock on the same global,
        // which can deadlock against a queued writer
        // (`install_global_keybindings`) — std::sync::RwLock is not
        // reentrant.
        {
            let kb = get_keybindings();
            let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

            // Tab — toggle scope when scoped models are present
            // (model-selector.ts:310-319).
            if read.matches_id(data, "tui.input.tab") {
                let mut state = lock(&self.state);
                if !state.scoped_model_items.is_empty() {
                    let next_scope = if state.scope == ModelScope::All {
                        ModelScope::Scoped
                    } else {
                        ModelScope::All
                    };
                    set_scope(&mut state, next_scope, &self.current_model);
                }
                return;
            }
            // Up arrow — wrap to bottom when at top (model-selector.ts:321-325).
            if read.matches_id(data, "tui.select.up") {
                let mut state = lock(&self.state);
                if state.filtered_models.is_empty() {
                    return;
                }
                state.selected_index = if state.selected_index == 0 {
                    state.filtered_models.len() - 1
                } else {
                    state.selected_index - 1
                };
                return;
            }
            // Down arrow — wrap to top when at bottom (model-selector.ts:327-331).
            if read.matches_id(data, "tui.select.down") {
                let mut state = lock(&self.state);
                if state.filtered_models.is_empty() {
                    return;
                }
                state.selected_index = if state.selected_index == state.filtered_models.len() - 1 {
                    0
                } else {
                    state.selected_index + 1
                };
                return;
            }
            // Enter (model-selector.ts:333-338).
            if read.matches_id(data, "tui.select.confirm") {
                let selected_model = {
                    let state = lock(&self.state);
                    state
                        .filtered_models
                        .get(state.selected_index)
                        .map(|item| item.model.clone())
                };
                if let Some(model) = selected_model {
                    self.handle_select(model);
                }
                return;
            }
            // Escape or Ctrl+C (model-selector.ts:340-343).
            if read.matches_id(data, "tui.select.cancel") {
                self.close();
                (self.on_cancel)();
                return;
            }
        }
        // Everything else goes to the search input (model-selector.ts:345-348).
        // The keybinding read guard was already dropped at the end of the
        // matching block above (Input::handle_input takes its own read lock).
        self.search_input.handle_input(data);
        let query = self.search_input.get_value().to_string();
        let mut state = lock(&self.state);
        state.search_value = query.clone();
        filter_models(&mut state, &query);
    }

    fn render(&self, width: usize) -> Vec<String> {
        let state = lock(&self.state);
        let mut lines: Vec<String> = Vec::new();

        // Container children (model-selector.ts:91-129): DynamicBorder,
        // Spacer(1), scope/hint (or warning hint), Spacer(1), search input,
        // Spacer(1), list, Spacer(1), DynamicBorder.
        lines.extend(self.top_border.render(width));
        lines.push(String::new());

        if state.scoped_models.is_empty() {
            // No scoped models: warning hint (model-selector.ts:102-104).
            // Text wraps it like the upstream `Text` child.
            lines.extend(
                Text::new(
                    self.theme.fg(
                        "warning",
                        "Only showing models from configured providers. Use /login to add providers.",
                    ),
                    0,
                    0,
                    None,
                )
                .render(width),
            );
        } else {
            lines.extend(
                Text::new(get_scope_text(&self.theme, state.scope), 0, 0, None).render(width),
            );
            lines.extend(Text::new(get_scope_hint_text(&self.theme), 0, 0, None).render(width));
        }
        lines.push(String::new());

        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        // List container (`updateList`, model-selector.ts:245-306).
        const MAX_VISIBLE: usize = 10;
        let len = state.filtered_models.len();
        let start_index = len
            .saturating_sub(MAX_VISIBLE)
            .min(state.selected_index.saturating_sub(MAX_VISIBLE / 2));
        let end_index = (start_index + MAX_VISIBLE).min(len);
        for i in start_index..end_index {
            let item = &state.filtered_models[i];
            let is_selected = i == state.selected_index;
            let is_current = models_are_equal(self.current_model.as_ref(), Some(&item.model));
            let line = if is_selected {
                let prefix = self.theme.fg("accent", "→ ");
                let model_text = self.theme.fg("accent", &item.id);
                let provider_badge = self.theme.fg("muted", &format!("[{}]", item.provider));
                let checkmark = if is_current {
                    self.theme.fg("success", " ✓")
                } else {
                    String::new()
                };
                format!("{prefix}{model_text} {provider_badge}{checkmark}")
            } else {
                let model_text = format!("  {}", item.id);
                let provider_badge = self.theme.fg("muted", &format!("[{}]", item.provider));
                let checkmark = if is_current {
                    self.theme.fg("success", " ✓")
                } else {
                    String::new()
                };
                format!("{model_text} {provider_badge}{checkmark}")
            };
            lines.push(truncate_to_width(&line, width, "...", false));
        }

        // Scroll indicator (model-selector.ts:281-284).
        if start_index > 0 || end_index < len {
            let scroll_info = self.theme.fg(
                "muted",
                &format!("  ({}/{})", state.selected_index + 1, len),
            );
            lines.push(truncate_to_width(&scroll_info, width, "", false));
        }

        // Error message or "no results" or selected model name
        // (model-selector.ts:287-299). Error lines wrap like the upstream
        // `Text` children.
        if let Some(error) = &state.error_message {
            for line in error.split('\n') {
                lines.extend(Text::new(self.theme.fg("error", line), 0, 0, None).render(width));
            }
        } else if state.filtered_models.is_empty() {
            lines.push(self.theme.fg("muted", "  No matching models"));
        } else {
            let selected = &state.filtered_models[state.selected_index];
            lines.push(String::new());
            lines.push(truncate_to_width(
                &self
                    .theme
                    .fg("muted", &format!("  Model Name: {}", selected.model.name)),
                width,
                "...",
                false,
            ));
        }

        // Refresh status line (model-selector.ts:300-305).
        if !state.refresh_status_message.is_empty() {
            lines.push(String::new());
            let color = if state.refresh_status_success {
                "success"
            } else {
                "muted"
            };
            lines.push(
                self.theme
                    .fg(color, &format!("  {}", state.refresh_status_message)),
            );
        }

        lines.push(String::new());
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn invalidate(&mut self) {
        self.search_input.invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ModelSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        // Propagate to the search input for IME cursor positioning
        // (model-selector.ts:39-46).
        self.search_input.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use pir_tui::tui::Tui;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pir-model-selector-test-{}-{nanos}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    /// Install the global 73-entry keybindings table (tui.select.*,
    /// tui.input.tab, ...).
    fn install_keybindings() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
    }

    /// Strip ANSI escape sequences (shared with config_selector tests).
    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                let mut out = String::with_capacity(line.len());
                let mut chars = line.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '\x1b' && chars.peek() == Some(&'[') {
                        chars.next();
                        for c in chars.by_ref() {
                            if c == 'm' {
                                break;
                            }
                        }
                    } else {
                        out.push(c);
                    }
                }
                out
            })
            .collect()
    }

    const ENV_KEY: &str = "PIR_TEST_MODEL_SELECTOR_KEY";

    /// Two providers (alpha, beta), three models; beta/b1 is the current
    /// model in most tests.
    const MODELS_JSON: &str = r#"{"providers": {
        "alpha": {
            "baseUrl": "https://api.example.com/v1",
            "api": "openai-completions",
            "apiKey": "PIR_TEST_MODEL_SELECTOR_KEY",
            "models": [{"id": "a1", "name": "Alpha One"}]
        },
        "beta": {
            "baseUrl": "https://api.example.com/v1",
            "api": "openai-completions",
            "apiKey": "PIR_TEST_MODEL_SELECTOR_KEY",
            "models": [
                {"id": "b1", "name": "Beta One"},
                {"id": "b2", "name": "Beta Two"}
            ]
        }
    }}"#;

    async fn runtime_with_models_json(models_json: &str) -> (TempDir, Arc<ModelRuntime>) {
        std::env::set_var(ENV_KEY, "test-api-key");
        let tmp = TempDir::new();
        let agent_dir = tmp.0.join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(agent_dir.join("models.json"), models_json).expect("write models.json");
        let runtime = ModelRuntime::create(crate::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: crate::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
        })
        .await;
        (tmp, runtime)
    }

    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn tui() -> Tui {
        Tui::new(Box::new(
            crate::modes::interactive::test_support::TestTerminal::new(),
        ))
    }

    /// Capture calls through a shared mutex for callback assertions.
    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn capture_model() -> (
        Arc<Mutex<Vec<(String, String)>>>,
        Box<dyn FnMut(Model) + Send>,
    ) {
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();
        (
            calls,
            Box::new(move |model: Model| {
                calls_clone.lock().unwrap().push((model.provider, model.id));
            }),
        )
    }

    /// The model rows of a render: lines with a cursor/indent prefix and a
    /// `[provider]` badge (scroll indicator and model-name line excluded).
    fn list_rows(rendered: &[String]) -> Vec<String> {
        plain(rendered.to_vec())
            .into_iter()
            .filter(|line| line.starts_with("→ ") || line.starts_with("  "))
            .filter(|line| line.contains('['))
            .collect()
    }

    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn build(
        current: Option<Model>,
        runtime: Arc<ModelRuntime>,
        scoped: Vec<(Model, Option<ModelThinkingLevel>)>,
        initial_search: Option<String>,
        on_cancel: Option<Box<dyn FnMut() + Send>>,
    ) -> (
        ModelSelectorComponent,
        Arc<Mutex<Vec<(String, String)>>>,
        Arc<Mutex<Vec<(String, String)>>>,
        Arc<Mutex<usize>>,
    ) {
        let (selected_calls, on_select) = capture_model();
        let (saved_calls, _save_default) = capture_model();
        let saved_calls_clone = saved_calls.clone();
        let cancels = Arc::new(Mutex::new(0usize));
        let cancels_clone = cancels.clone();
        let component = ModelSelectorComponent::new(
            current,
            runtime,
            scoped,
            theme(),
            tui(),
            Box::new(move |model: &Model| {
                saved_calls_clone
                    .lock()
                    .unwrap()
                    .push((model.provider.clone(), model.id.clone()));
            }),
            on_select,
            on_cancel.unwrap_or_else(|| {
                Box::new(move || {
                    *cancels_clone.lock().unwrap() += 1;
                })
            }),
            initial_search,
        );
        (component, selected_calls, saved_calls, cancels)
    }

    #[tokio::test]
    async fn current_model_sorted_first_with_checkmark() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(MODELS_JSON).await;
        let current = runtime.get_model("beta", "b1").expect("b1");
        let (component, _, _, _) = build(Some(current), runtime, Vec::new(), None, None);
        let lines = plain(component.render(80));
        // Row order: current (b1) first, then providers alphabetically
        // (alpha before beta).
        let rows: Vec<String> = list_rows(&lines);
        assert_eq!(rows[0], "→ b1 [beta] ✓");
        assert_eq!(rows[1], "  a1 [alpha]");
        assert_eq!(rows[2], "  b2 [beta]");
        // Selected model name line follows the rows.
        assert!(lines.join("\n").contains("Model Name: Beta One"));
    }

    #[tokio::test]
    async fn search_filters_and_enter_selects() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(MODELS_JSON).await;
        let (mut component, selected_calls, saved_calls, _) =
            build(None, runtime, Vec::new(), None, None);
        // Type "b2" — matches only beta/b2 (getModelSelectorSearchText leads
        // with the provider).
        component.handle_input("b");
        component.handle_input("2");
        let rows = list_rows(&component.render(80));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], "→ b2 [beta]");
        // Enter selects the filtered item.
        component.handle_input("\r");
        let selected = selected_calls.lock().unwrap();
        assert_eq!(*selected, vec![("beta".to_string(), "b2".to_string())]);
        // The default was saved through the save_default hook first.
        assert_eq!(
            *saved_calls.lock().unwrap(),
            vec![("beta".to_string(), "b2".to_string())]
        );
    }

    #[tokio::test]
    async fn initial_search_input_filters_at_construction() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(MODELS_JSON).await;
        // Provider-prefixed query ("alpha/a1" → tokens alpha + a1): only
        // alpha/a1 matches (a bare "a1" also matches beta/b1 through the
        // fuzzy transposition fallback, fuzzy.ts:65-86).
        let (component, _, _, _) = build(
            None,
            runtime,
            Vec::new(),
            Some("alpha/a1".to_string()),
            None,
        );
        assert_eq!(component.get_search_input().get_value(), "alpha/a1");
        let rows = list_rows(&component.render(80));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], "→ a1 [alpha]");
    }

    #[tokio::test]
    async fn up_down_wrap_around() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(MODELS_JSON).await;
        let (mut component, _, _, _) = build(None, runtime, Vec::new(), None, None);
        // Up at the top wraps to the last row.
        component.handle_input("\x1b[A");
        let rows = list_rows(&component.render(80));
        assert_eq!(rows[2], "→ b2 [beta]");
        // Down wraps back to the first row.
        component.handle_input("\x1b[B");
        let rows = list_rows(&component.render(80));
        assert_eq!(rows[0], "→ a1 [alpha]");
    }

    #[tokio::test]
    async fn tab_toggles_scope_when_scoped_models_present() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(MODELS_JSON).await;
        let scoped_model = runtime.get_model("alpha", "a1").expect("a1");
        let (mut component, _, _, _) = build(
            None,
            runtime,
            vec![(scoped_model, Some(ModelThinkingLevel::Low))],
            None,
            None,
        );
        // Default scope is "scoped" when scoped models exist
        // (model-selector.ts:87); only the scoped item is listed.
        let lines = plain(component.render(80));
        assert!(lines.join("\n").contains("(all/scoped)"));
        let rows = list_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], "→ a1 [alpha]");
        // Tab switches to "all" (full catalog).
        component.handle_input("\t");
        let rows = list_rows(&component.render(80));
        assert_eq!(rows.len(), 3);
        // Tab switches back to "scoped".
        component.handle_input("\t");
        let rows = list_rows(&component.render(80));
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn escape_cancels_and_closes() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(MODELS_JSON).await;
        let (mut component, _, _, cancels) = build(None, runtime, Vec::new(), None, None);
        component.handle_input("\x1b");
        assert_eq!(*cancels.lock().unwrap(), 1);
        // The component is closed; the in-flight refresh (if any) no longer
        // applies. There is no public closed flag — the cancel callback is
        // the observable contract.
    }

    #[tokio::test]
    async fn refresh_reloads_catalog_and_reports_status() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {
                "beta": {
                    "baseUrl": "https://api.example.com/v1",
                    "api": "openai-completions",
                    "apiKey": "PIR_TEST_MODEL_SELECTOR_KEY",
                    "models": [{"id": "b1", "name": "Beta One"}]
                }
            }}"#,
        )
        .await;
        let (component, _, _, _) = build(None, runtime.clone(), Vec::new(), None, None);
        // Add a provider while the background refresh is still queued: the
        // constructor's spawn runs on the next await, so it must observe the
        // new catalog (model-selector.ts:186 `loadModelsFromSnapshot`).
        let agent_dir = _tmp.0.join("agent");
        std::fs::write(agent_dir.join("models.json"), MODELS_JSON).expect("rewrite models.json");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let lines = plain(component.render(80)).join("\n");
            if lines.contains("Model catalogs refreshed.") && lines.contains("a1") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "refresh never applied: {lines}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn refresh_reports_catalog_errors() {
        install_keybindings();
        let (_tmp, runtime) = runtime_with_models_json(
            r#"{"providers": {
                "alpha": {
                    "baseUrl": "https://api.example.com/v1",
                    "api": "openai-completions",
                    "apiKey": "PIR_TEST_MODEL_SELECTOR_KEY",
                    "models": [{"id": "a1", "name": "Alpha One"}]
                },
                "broken": {
                    "baseUrl": "https://api.example.com/v1",
                    "apiKey": "PIR_TEST_MODEL_SELECTOR_KEY",
                    "models": [{"id": "x1"}]
                }
            }}"#,
        )
        .await;
        assert!(runtime.get_error().is_some(), "broken provider must error");
        let (component, _, _, _) = build(None, runtime, Vec::new(), None, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let lines = plain(component.render(80)).join("\n");
            // get_error reports the composition error; the success status
            // must not appear.
            if lines.contains("no \"api\" specified") {
                assert!(!lines.contains("Model catalogs refreshed."));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "error status never applied: {lines}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn scroll_indicator_shows_when_list_overflows() {
        install_keybindings();
        // 11 models — maxVisible 10 — so the scroll indicator appears and
        // the row count is capped at 10.
        let mut models_json = String::new();
        for i in 1..=11 {
            models_json.push_str(&format!(r#"{{"id": "m{i}", "name": "Model {i}"}},"#));
        }
        models_json.pop(); // trailing comma
        let json = format!(
            r#"{{"providers": {{"alpha": {{
                "baseUrl": "https://api.example.com/v1",
                "api": "openai-completions",
                "apiKey": "PIR_TEST_MODEL_SELECTOR_KEY",
                "models": [{models_json}]
            }}}}}}"#
        );
        let (_tmp, runtime) = runtime_with_models_json(&json).await;
        let (component, _, _, _) = build(None, runtime, Vec::new(), None, None);
        let lines = plain(component.render(40));
        let joined = lines.join("\n");
        assert!(joined.contains("(1/11)"));
        let rows = lines
            .iter()
            .filter(|line| line.starts_with("→ ") || line.starts_with("  "))
            .filter(|line| line.contains('['))
            .count();
        assert_eq!(rows, 10);
    }
}
