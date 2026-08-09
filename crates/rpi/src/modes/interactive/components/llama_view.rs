//! Port of `packages/coding-agent/src/extensions/llama/ui.ts`
//! @ pi 0.82.1 (2efa728) — the `/llama` TUI: model list, select/confirm
//! frames, the Hugging Face search component (debounced, cached, fuzzy
//! filtered), and the progress view with stop-confirmation support.
//!
//! The component ([`LlamaViewComponent`]) is the input/render half mounted
//! in place of the editor; [`LlamaViewUi`] is the async half driven by
//! `run_llama_manager` (extensions/llama/mod.rs). They share
//! [`LlamaViewState`]; request resolutions travel through per-request
//! oneshot channels held by the content variants (never through the state
//! lock, so input callbacks cannot deadlock).
//!
//! Intentional differences:
//! - `ctx.ui.custom` mounting becomes the caller's `show_selector` +
//!   `spawn_async` (the extension-host custom-UI hook is T15, D-047).
//! - The 500ms search debounce (`setTimeout`) becomes a spawned task gated
//!   by a generation counter; stale results are dropped like upstream's
//!   `this.query !== query` check.
//! - `localeCompare` ordering becomes plain string ordering (model ids and
//!   quant names are ASCII in practice).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rpi_tui::components::input::Input;
use rpi_tui::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use rpi_tui::fuzzy::fuzzy_filter;
use rpi_tui::keybindings::{get_keybindings, KeybindingsManager};
use rpi_tui::tui::{Component, Focusable, RenderHandle};
use rpi_tui::utils::{truncate_to_width, visible_width};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::themes::Theme;
use crate::extensions::llama::client::{LlamaModelInfo, LlamaModelStatusValue};
use crate::extensions::llama::huggingface::HuggingFaceModel;
use crate::extensions::llama::{HuggingFaceSearchFn, LlamaManagerAction, LlamaUi, ProgressState};

use super::keybinding_hints::key_hint;

/// `DOWNLOAD_VALUE` (ui.ts:23).
const DOWNLOAD_VALUE: &str = "\0download";

/// The exact `owner/repository[:quant]` pattern (ui.ts:259).
const EXACT_MODEL_PATTERN: &str = r"^[^/\s]+/[^:\s]+(?::[^\s:]+)?$";

/// Compiled once (the upstream literal regex; T14 review m2).
static EXACT_MODEL_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(EXACT_MODEL_PATTERN).expect("EXACT_MODEL_PATTERN is a valid regex")
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `contextLabel` (ui.ts:32-42).
fn context_label(model: &LlamaModelInfo) -> Option<String> {
    let format = |context: u64| {
        if context >= 1000 {
            format!("{}k", (context as f64 / 1000.0).round() as u64)
        } else {
            context.to_string()
        }
    };
    if let Some(meta) = &model.meta {
        if let Some(context) = meta.n_ctx.or(meta.n_ctx_train) {
            return Some(format(context));
        }
    }
    let args = &model.status.args;
    for index in 0..args.len().saturating_sub(1) {
        let flag = &args[index];
        if flag != "--ctx-size" && flag != "-c" && flag != "-ctx" {
            continue;
        }
        if let Ok(value) = args[index + 1].parse::<u64>() {
            if value > 0 {
                return Some(format(value));
            }
        }
    }
    None
}

/// `modelDescription` (ui.ts:44-52).
fn model_description(model: &LlamaModelInfo) -> String {
    let mut details: Vec<String> = Vec::new();
    let loaded = model.is_loaded();
    if loaded {
        details.push("loaded".to_owned());
    } else if model.status.value != LlamaModelStatusValue::UNLOADED {
        details.push(model.status.value.clone());
    }
    if loaded {
        if let Some(context) = context_label(model) {
            details.push(format!("{context} context"));
        }
    }
    details.join(" · ")
}

/// `compactCount` (ui.ts:90-94).
fn compact_count(value: i64) -> String {
    if value >= 1_000_000 {
        let millions = value as f64 / 1_000_000.0;
        if value >= 10_000_000 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if value >= 1_000 {
        let thousands = value as f64 / 1_000.0;
        if value >= 100_000 {
            format!("{thousands:.0}k")
        } else {
            format!("{thousands:.1}k")
        }
    } else {
        value.to_string()
    }
}

/// `selectTheme` (ui.ts:54-62).
fn select_theme(theme: &Arc<Theme>) -> Arc<rpi_tui::components::select_list::SelectListTheme> {
    let accent_prefix = Arc::clone(theme);
    let accent_text = Arc::clone(theme);
    let muted = Arc::clone(theme);
    let dim = Arc::clone(theme);
    let warning = Arc::clone(theme);
    Arc::new(rpi_tui::components::select_list::SelectListTheme {
        selected_prefix: Box::new(move |text| accent_prefix.fg("accent", text)),
        selected_text: Box::new(move |text| accent_text.fg("accent", text)),
        description: Box::new(move |text| muted.fg("muted", text)),
        scroll_info: Box::new(move |text| dim.fg("dim", text)),
        no_match: Box::new(move |text| warning.fg("warning", text)),
    })
}

/// The Hugging Face search component (`HuggingFaceSearch`, ui.ts:96-274).
struct HuggingFaceSearch {
    input: Input,
    results: Vec<HuggingFaceModel>,
    filtered_results: Vec<HuggingFaceModel>,
    selected_index: usize,
    query: String,
    status: String,
    /// Lowercase-query result cache (`LlamaView.searchCache`, ui.ts:280) —
    /// inherited across searches within one mounted view.
    cache: HashMap<String, Vec<HuggingFaceModel>>,
    /// Debounce generation: bumped on every query change; stale search
    /// completions drop their results (ui.ts:220 `this.query !== query`).
    generation: u64,
    /// In-flight search cancellation (`this.request`, ui.ts:111).
    request_token: Option<CancellationToken>,
    search_fn: HuggingFaceSearchFn,
    shared: Arc<Mutex<LlamaViewState>>,
    render_handle: RenderHandle,
    resolution: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>,
}

impl HuggingFaceSearch {
    fn new(
        search_fn: HuggingFaceSearchFn,
        shared: Arc<Mutex<LlamaViewState>>,
        render_handle: RenderHandle,
        cache: HashMap<String, Vec<HuggingFaceModel>>,
        resolution: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>,
    ) -> Self {
        HuggingFaceSearch {
            input: Input::new(),
            results: Vec::new(),
            filtered_results: Vec::new(),
            selected_index: 0,
            query: String::new(),
            status: "Type at least 2 characters".to_owned(),
            cache,
            generation: 0,
            request_token: None,
            search_fn,
            shared,
            render_handle,
            resolution,
        }
    }

    fn resolve(&self, model: Option<String>) {
        if let Some(token) = &self.request_token {
            token.cancel();
        }
        if let Some(sender) = lock(&self.resolution).take() {
            let _ = sender.send(model);
        }
    }

    /// `filterResults` (ui.ts:182-191).
    fn filter_results(&mut self) {
        if self.query.is_empty() {
            self.filtered_results = self.results.clone();
        } else {
            let matches: std::collections::HashSet<String> = fuzzy_filter(
                self.results.clone(),
                &self.query,
                |model: &HuggingFaceModel| model.id.clone(),
            )
            .into_iter()
            .map(|model| model.id)
            .collect();
            self.filtered_results = self
                .results
                .iter()
                .filter(|model| matches.contains(&model.id))
                .cloned()
                .collect();
        }
        self.selected_index = self
            .selected_index
            .min(self.filtered_results.len().saturating_sub(1));
    }

    /// `scheduleSearch` + `runSearch` (ui.ts:193-233): the 500ms debounce is
    /// a spawned task; a stale generation or changed query drops the result.
    fn schedule_search(&mut self) {
        if let Some(token) = self.request_token.take() {
            token.cancel();
        }
        self.generation += 1;
        let generation = self.generation;
        if self.query.len() < 2 {
            self.status = "Type at least 2 characters".to_owned();
            self.filter_results();
            return;
        }
        if let Some(cached) = self.cache.get(&self.query.to_lowercase()) {
            self.results = cached.clone();
            self.selected_index = 0;
            self.status = if self.results.is_empty() {
                "No GGUF models found".to_owned()
            } else {
                String::new()
            };
            self.filter_results();
            return;
        }
        self.status = "Searching Hugging Face…".to_owned();
        self.filter_results();
        let query = self.query.clone();
        let token = CancellationToken::new();
        self.request_token = Some(token.clone());
        let search_fn = self.search_fn.clone();
        let shared = Arc::clone(&self.shared);
        let render_handle = self.render_handle.clone();
        tokio::spawn(async move {
            let debounce = tokio::time::sleep(std::time::Duration::from_millis(500));
            tokio::select! {
                () = token.cancelled() => {}
                () = debounce => {
                    let result = search_fn(query.clone(), token.clone()).await;
                    let mut state = lock(&shared);
                    let LlamaContent::Search(search) = &mut state.content else {
                        return;
                    };
                    // `this.closed || request.signal.aborted || this.query !==
                    // query` (ui.ts:220, 226).
                    if search.generation != generation
                        || search.query != query
                        || (token.is_cancelled() && result.is_ok())
                    {
                        return;
                    }
                    match result {
                        Ok(results) => {
                            search
                                .cache
                                .insert(query.to_lowercase(), results.clone());
                            search.selected_index = 0;
                            search.status = if results.is_empty() {
                                "No GGUF models found".to_owned()
                            } else {
                                String::new()
                            };
                            search.results = results;
                        }
                        Err(error) => {
                            if token.is_cancelled() {
                                return;
                            }
                            search.results = Vec::new();
                            search.status = error.message;
                        }
                    }
                    search.filter_results();
                    drop(state);
                    render_handle.request_render();
                }
            }
        });
    }

    /// `handleInput` (ui.ts:243-273).
    fn handle_input(&mut self, data: &str, keybindings: &KeybindingsManager) {
        if keybindings.matches_id(data, "tui.select.up") {
            if !self.filtered_results.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_results.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
            return;
        }
        if keybindings.matches_id(data, "tui.select.down") {
            if !self.filtered_results.is_empty() {
                self.selected_index = if self.selected_index == self.filtered_results.len() - 1 {
                    0
                } else {
                    self.selected_index + 1
                };
            }
            return;
        }
        if keybindings.matches_id(data, "tui.select.confirm") {
            let exact = EXACT_MODEL_RE.is_match(&self.query);
            let selected = if exact {
                Some(self.query.clone())
            } else {
                self.filtered_results
                    .get(self.selected_index)
                    .map(|model| model.id.clone())
            };
            if let Some(selected) = selected {
                self.resolve(Some(selected));
            }
            return;
        }
        if keybindings.matches_id(data, "tui.select.cancel") {
            self.resolve(None);
            return;
        }
        self.input.handle_input(data);
        let query = self.input.get_value().trim().to_owned();
        if query == self.query {
            return;
        }
        self.query = query;
        self.schedule_search();
    }

    /// `updateResults` (ui.ts:146-180).
    fn render(&self, theme: &Theme, width: usize) -> Vec<String> {
        let mut lines = vec![theme.fg("dim", "Model name or owner/repository[:quant]")];
        lines.extend(self.input.render(width));
        lines.push(String::new());
        let max_visible = 10;
        let start = self
            .selected_index
            .saturating_sub(max_visible / 2)
            .min(self.filtered_results.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.filtered_results.len());
        for (index, model) in self
            .filtered_results
            .iter()
            .enumerate()
            .take(end)
            .skip(start)
        {
            let details = format!("{} downloads", compact_count(model.downloads));
            if index == self.selected_index {
                lines.push(theme.fg("accent", &format!("→ {}  {details}", model.id)));
            } else {
                lines.push(format!(
                    "  {}{}",
                    model.id,
                    theme.fg("muted", &format!("  {details}"))
                ));
            }
        }
        if start > 0 || end < self.filtered_results.len() {
            lines.push(theme.fg(
                "dim",
                &format!(
                    "  ({}/{})",
                    self.selected_index + 1,
                    self.filtered_results.len()
                ),
            ));
        }
        if self.filtered_results.is_empty() || self.status == "Searching Hugging Face…" {
            lines.push(theme.fg("dim", &format!("  {}", self.status)));
        }
        lines
    }
}

/// View content variants (`LlamaView.setContent`, ui.ts:305-319).
enum LlamaContent {
    /// Initial "Loading…" placeholder (ui.ts:293).
    Loading,
    Models {
        server_url: String,
        list: SelectList,
    },
    Select {
        title: String,
        list: SelectList,
    },
    Search(Box<HuggingFaceSearch>),
    Progress(ProgressState),
    Status {
        title: String,
        message: String,
    },
}

/// Shared state between the mounted component and the async UI half.
struct LlamaViewState {
    content: LlamaContent,
    /// Progress-stop waiters (`progressPromise`/`progressResolver`,
    /// ui.ts:284-285): a key cancel during the progress view resolves every
    /// waiter.
    progress_waiters: Vec<oneshot::Sender<()>>,
}

/// The mounted `/llama` view (`LlamaView` as a `Component`/`Focusable`).
pub struct LlamaViewComponent {
    theme: Arc<Theme>,
    state: Arc<Mutex<LlamaViewState>>,
    focused: bool,
}

/// The async UI half driven by `run_llama_manager` (the `LlamaUi`
/// implementation, ui.ts:276-478).
pub struct LlamaViewUi {
    theme: Arc<Theme>,
    render_handle: RenderHandle,
    state: Arc<Mutex<LlamaViewState>>,
}

/// Create the mounted component plus its async handle.
pub fn new_llama_view(
    theme: Arc<Theme>,
    render_handle: RenderHandle,
) -> (LlamaViewComponent, LlamaViewUi) {
    let state = Arc::new(Mutex::new(LlamaViewState {
        content: LlamaContent::Loading,
        progress_waiters: Vec::new(),
    }));
    (
        LlamaViewComponent {
            theme: Arc::clone(&theme),
            state: Arc::clone(&state),
            focused: false,
        },
        LlamaViewUi {
            theme,
            render_handle,
            state,
        },
    )
}

impl LlamaViewUi {
    /// `setContent` (ui.ts:305-319): swap the view and request a render.
    fn set_content(&self, content: LlamaContent) {
        lock(&self.state).content = content;
        self.render_handle.request_render();
    }
}

#[async_trait::async_trait]
impl LlamaUi for LlamaViewUi {
    /// `showModels` (ui.ts:321-358): loaded models first, then by id; the
    /// "Download model…" entry is appended last.
    async fn show_models(
        &mut self,
        server_url: &str,
        models: Vec<LlamaModelInfo>,
    ) -> LlamaManagerAction {
        let mut sorted = models;
        sorted.sort_by(|left, right| {
            let loaded = (right.status.value == LlamaModelStatusValue::LOADED) as i32
                - (left.status.value == LlamaModelStatusValue::LOADED) as i32;
            loaded.cmp(&0).then_with(|| left.id.cmp(&right.id))
        });
        let by_id: HashMap<String, LlamaModelInfo> = sorted
            .iter()
            .map(|model| (model.id.clone(), model.clone()))
            .collect();
        let mut items: Vec<SelectItem> = sorted
            .iter()
            .map(|model| SelectItem {
                value: model.id.clone(),
                label: model.id.clone(),
                description: Some(model_description(model)),
            })
            .collect();
        items.push(SelectItem {
            value: DOWNLOAD_VALUE.to_owned(),
            label: "Download model…".to_owned(),
            description: Some("Hugging Face owner/repository[:quant]".to_owned()),
        });
        let resolution: Arc<Mutex<Option<oneshot::Sender<LlamaManagerAction>>>> =
            Arc::new(Mutex::new(None));
        let mut list = SelectList::new(
            items,
            12,
            select_theme(&self.theme),
            Some(SelectListLayoutOptions {
                min_primary_column_width: Some(36),
                max_primary_column_width: Some(56),
                truncate_primary: None,
            }),
        );
        let on_select_resolution = Arc::clone(&resolution);
        list.on_select = Some(Box::new(move |item: &SelectItem| {
            let action = if item.value == DOWNLOAD_VALUE {
                Some(LlamaManagerAction::Download)
            } else {
                by_id
                    .get(&item.value)
                    .map(|model| LlamaManagerAction::Model(Box::new(model.clone())))
            };
            if let (Some(action), Some(sender)) = (action, lock(&on_select_resolution).take()) {
                let _ = sender.send(action);
            }
        }));
        let on_cancel_resolution = Arc::clone(&resolution);
        list.on_cancel = Some(Box::new(move || {
            if let Some(sender) = lock(&on_cancel_resolution).take() {
                let _ = sender.send(LlamaManagerAction::Close);
            }
        }));
        let (tx, rx) = oneshot::channel();
        *lock(&resolution) = Some(tx);
        self.set_content(LlamaContent::Models {
            server_url: server_url.to_owned(),
            list,
        });
        rx.await.unwrap_or(LlamaManagerAction::Close)
    }

    /// `select` (ui.ts:360-379).
    async fn select(&mut self, title: &str, options: Vec<String>) -> Option<String> {
        let resolution: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>> =
            Arc::new(Mutex::new(None));
        let items: Vec<SelectItem> = options
            .iter()
            .map(|option| SelectItem {
                value: option.clone(),
                label: option.clone(),
                description: None,
            })
            .collect();
        let mut list = SelectList::new(items, 12, select_theme(&self.theme), None);
        let on_select_resolution = Arc::clone(&resolution);
        list.on_select = Some(Box::new(move |item: &SelectItem| {
            if let Some(sender) = lock(&on_select_resolution).take() {
                let _ = sender.send(Some(item.value.clone()));
            }
        }));
        let on_cancel_resolution = Arc::clone(&resolution);
        list.on_cancel = Some(Box::new(move || {
            if let Some(sender) = lock(&on_cancel_resolution).take() {
                let _ = sender.send(None);
            }
        }));
        let (tx, rx) = oneshot::channel();
        *lock(&resolution) = Some(tx);
        self.set_content(LlamaContent::Select {
            title: title.to_owned(),
            list,
        });
        rx.await.ok().flatten()
    }

    /// `searchModels` (ui.ts:390-413).
    async fn search_models(&mut self, search: HuggingFaceSearchFn) -> Option<String> {
        let resolution: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>> =
            Arc::new(Mutex::new(None));
        // The result cache survives within one mounted view (ui.ts:280).
        let cache = {
            let state = lock(&self.state);
            match &state.content {
                LlamaContent::Search(previous) => previous.cache.clone(),
                _ => HashMap::new(),
            }
        };
        let (tx, rx) = oneshot::channel();
        *lock(&resolution) = Some(tx);
        self.set_content(LlamaContent::Search(Box::new(HuggingFaceSearch::new(
            search,
            Arc::clone(&self.state),
            self.render_handle.clone(),
            cache,
            resolution,
        ))));
        rx.await.ok().flatten()
    }

    /// `showStatus` (ui.ts:415-417).
    fn show_status(&mut self, title: &str, message: &str) {
        self.set_content(LlamaContent::Status {
            title: title.to_owned(),
            message: message.to_owned(),
        });
    }

    /// `progress` (ui.ts:419-428): switch to the progress view (once) and
    /// resolve when the user presses the cancel key.
    async fn progress(&mut self, state: &ProgressState) {
        let (tx, rx) = oneshot::channel();
        {
            let mut shared = lock(&self.state);
            if !matches!(shared.content, LlamaContent::Progress(_)) {
                shared.content = LlamaContent::Progress(state.clone());
            }
            shared.progress_waiters.push(tx);
        }
        self.render_handle.request_render();
        let _ = rx.await;
    }

    /// `updateProgress` (ui.ts:430-455).
    fn update_progress(&mut self, state: &ProgressState) {
        {
            let mut shared = lock(&self.state);
            if let LlamaContent::Progress(current) = &mut shared.content {
                *current = state.clone();
            } else {
                return;
            }
        }
        self.render_handle.request_render();
    }
}

impl LlamaViewComponent {
    /// `frame` (ui.ts:64-75).
    fn frame(&self, title: &str, body: Vec<String>, footer: Option<String>) -> Vec<String> {
        let theme = &self.theme;
        let mut lines = Vec::new();
        // The width is applied by the caller's render pass; the border is
        // padded there (see `render`).
        lines.push(theme.fg("accent", &Theme::bold(title)));
        lines.extend(body);
        if let Some(footer) = footer {
            lines.push(String::new());
            lines.push(theme.fg("dim", &footer));
        }
        lines
    }
}

impl Component for LlamaViewComponent {
    /// `render` (ui.ts:469-474): frame the active content and clip
    /// over-wide lines.
    fn render(&self, width: usize) -> Vec<String> {
        let state = lock(&self.state);
        let theme = &self.theme;
        let (title, mut body, footer): (String, Vec<String>, Option<String>) = match &state.content
        {
            LlamaContent::Loading => (
                "llama.cpp models".to_owned(),
                vec![theme.fg("muted", "Loading…")],
                None,
            ),
            LlamaContent::Models {
                server_url, list, ..
            } => {
                let mut body = vec![theme.fg("dim", server_url), String::new()];
                body.extend(list.render(width));
                (
                    "llama.cpp models".to_owned(),
                    body,
                    Some(format!(
                        "{} • {}",
                        key_hint(theme, "tui.select.confirm", "load/unload/download"),
                        key_hint(theme, "tui.select.cancel", "close")
                    )),
                )
            }
            LlamaContent::Select { title, list, .. } => {
                let mut body = vec![String::new()];
                body.extend(list.render(width));
                (
                    title.clone(),
                    body,
                    Some(format!(
                        "{} • {}",
                        key_hint(theme, "tui.select.confirm", "select"),
                        key_hint(theme, "tui.select.cancel", "cancel")
                    )),
                )
            }
            LlamaContent::Search(search) => {
                let mut body = vec![String::new()];
                body.extend(search.render(theme, width));
                (
                    "Download model".to_owned(),
                    body,
                    Some(format!(
                        "{} • {}",
                        key_hint(theme, "tui.select.confirm", "select"),
                        key_hint(theme, "tui.select.cancel", "back")
                    )),
                )
            }
            LlamaContent::Progress(progress) => {
                let mut body = vec![
                    theme.fg("text", &progress.model),
                    String::new(),
                    theme.fg("muted", &progress.message),
                ];
                if let Some(ratio) = progress.ratio {
                    let available = 40;
                    let clamped = ratio.clamp(0.0, 1.0);
                    let filled = (clamped * available as f64).round() as usize;
                    body.push(theme.fg(
                        "accent",
                        &format!(
                            "{}{} {}%",
                            "█".repeat(filled),
                            "─".repeat(available - filled),
                            (clamped * 100.0).round() as u64
                        ),
                    ));
                }
                if let Some(detail) = &progress.detail {
                    body.push(theme.fg("dim", detail));
                }
                (
                    progress.title.clone(),
                    body,
                    Some(key_hint(theme, "tui.select.cancel", "stop")),
                )
            }
            LlamaContent::Status { title, message } => (
                title.clone(),
                vec![String::new(), theme.fg("muted", message)],
                None,
            ),
        };
        let mut lines = vec![theme.fg("accent", &"─".repeat(width.max(1)))];
        lines.extend(self.frame(&title, std::mem::take(&mut body), footer));
        lines.push(theme.fg("accent", &"─".repeat(width.max(1))));
        lines
            .into_iter()
            .map(|line| {
                if visible_width(&line) > width {
                    truncate_to_width(&line, width, "", false)
                } else {
                    line
                }
            })
            .collect()
    }

    /// `handleInput` (ui.ts:457-467): during progress only the cancel key is
    /// handled (it resolves the stop waiters); everything else routes to the
    /// mounted list/search.
    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        let keybindings = keybindings.read().unwrap_or_else(|e| e.into_inner());
        let mut state = lock(&self.state);
        match &mut state.content {
            LlamaContent::Progress(_) => {
                if keybindings.matches_id(data, "tui.select.cancel") {
                    for waiter in state.progress_waiters.drain(..) {
                        let _ = waiter.send(());
                    }
                }
            }
            LlamaContent::Models { list, .. } | LlamaContent::Select { list, .. } => {
                list.handle_input(data);
            }

            LlamaContent::Search(search) => search.handle_input(data, &keybindings),
            LlamaContent::Loading | LlamaContent::Status { .. } => {}
        }
    }

    fn invalidate(&mut self) {}
}

impl Focusable for LlamaViewComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use crate::extensions::llama::client::{LlamaArchitecture, LlamaModelMeta, LlamaModelStatus};

    use super::*;

    fn model(id: &str, status: &str) -> LlamaModelInfo {
        LlamaModelInfo {
            id: id.to_owned(),
            status: LlamaModelStatus {
                value: status.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// `modelDescription` + `contextLabel` (ui.ts:32-52).
    #[test]
    fn model_description_matches_upstream_rules() {
        let mut loaded = model("m", "loaded");
        loaded.meta = Some(LlamaModelMeta {
            n_ctx: Some(32768),
            ..Default::default()
        });
        assert_eq!(model_description(&loaded), "loaded · 33k context");

        let mut sleeping = model("m", "sleeping");
        sleeping.meta = Some(LlamaModelMeta {
            n_ctx_train: Some(8192),
            ..Default::default()
        });
        assert_eq!(model_description(&sleeping), "loaded · 8k context");

        // Context from launch args when meta is absent (ui.ts:35-41).
        let mut args_model = model("m", "loaded");
        args_model.status.args = vec![
            "llama-server".to_owned(),
            "-c".to_owned(),
            "65536".to_owned(),
        ];
        assert_eq!(model_description(&args_model), "loaded · 66k context");

        assert_eq!(model_description(&model("m", "unloaded")), "");
        assert_eq!(model_description(&model("m", "loading")), "loading");
        assert_eq!(model_description(&model("m", "downloading")), "downloading");
    }

    /// `compactCount` (ui.ts:90-94).
    #[test]
    fn compact_count_thresholds() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1200), "1.2k");
        assert_eq!(compact_count(100_000), "100k");
        assert_eq!(compact_count(1_200_000), "1.2M");
        assert_eq!(compact_count(12_000_000), "12M");
    }

    /// `showModels` ordering (ui.ts:322-325): loaded first, then by id.
    #[test]
    fn show_models_sorts_loaded_first_then_by_id() {
        let mut models = [
            model("zeta", "unloaded"),
            model("alpha", "loaded"),
            model("beta", "unloaded"),
            model("omega", "loaded"),
        ];
        models.sort_by(|left, right| {
            let loaded = (right.status.value == LlamaModelStatusValue::LOADED) as i32
                - (left.status.value == LlamaModelStatusValue::LOADED) as i32;
            loaded.cmp(&0).then_with(|| left.id.cmp(&right.id))
        });
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "omega", "beta", "zeta"]);
    }

    /// Input modalities drive the image marker in `to_pi_model` — the
    /// description only surfaces context; `is_loaded` covers sleeping
    /// (index.ts:7-9).
    #[test]
    fn is_loaded_covers_sleeping() {
        assert!(model("m", "sleeping").is_loaded());
        assert!(model("m", "loaded").is_loaded());
        assert!(!model("m", "loading").is_loaded());
        let mut arch = model("m", "loaded");
        arch.architecture = Some(LlamaArchitecture {
            input_modalities: vec!["text".to_owned(), "image".to_owned()],
            output_modalities: Vec::new(),
        });
        assert!(arch.is_loaded());
    }
}
