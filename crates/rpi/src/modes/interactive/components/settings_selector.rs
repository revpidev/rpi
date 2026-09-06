//! Port of `settings-selector.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Upstream passes a `SettingsConfig` + a `SettingsCallbacks` object with
//!   28 `on*Change` callbacks; the port takes [`SettingsSelectorOptions`]
//!   (same fields as upstream `SettingsConfig`) plus a single
//!   `on_change: Box<dyn FnMut(SettingsChange) + Send>` where
//!   [`SettingsChange`] carries one variant per setting (all 28), plus
//!   `on_cancel`. The optional upstream `onThemePreview` becomes
//!   [`SettingsChange::ThemePreview`] — the integration layer can ignore it.
//! - `terminalTheme` is [`TerminalColorScheme`] (Dark/Light, upstream
//!   `"dark" | "light"`).
//! - `supportsImages` comes from the local `rpi_tui::terminal_image::
//!   get_capabilities()` (upstream `getCapabilities().images`).
//! - The 0.82.1 panel has no `on_open_*` callback hooks and no list
//!   grouping: settings with choices are `SettingItem.values` cycles, and
//!   complex settings open submenus through the `SettingItem.submenu`
//!   factory. Submenu navigation is the "group switching" the panel offers.
//! - The upstream `ThemeSubmenu` swaps whole child trees (`setContent`);
//!   the port keeps the same state machine in one boxed component slot
//!   behind an `Arc<Mutex<...>>` (Rust self-referential-closure pattern) and
//!   takes the component out of the slot while dispatching input, so a
//!   mode switch during `handle_input` replaces the UI like upstream.
//! - `THINKING_DESCRIPTIONS` has no "off" entry (the local `ThinkingLevel`
//!   has no `Off` variant; upstream's available levels never include it).
//! - `double-escape-action` parses all three values ("tree"/"fork"/"none");
//!   upstream casts `newValue as "fork" | "tree"` (settings-selector.ts:797)
//!   which would silently pass "none" through.

use std::sync::{Arc, Mutex};

use rpi_agent::types::QueueMode;
use rpi_ai::types::{ThinkingLevel, Transport};
use rpi_tui::components::select_list::{
    SelectItem, SelectItemFn, SelectList, SelectListLayoutOptions, SelectListTheme,
};
use rpi_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme, SubmenuDone, SubmenuFactory,
};
use rpi_tui::terminal_colors::TerminalColorScheme;
use rpi_tui::terminal_image::get_capabilities;
use rpi_tui::tui::{Component, Focusable};

use crate::core::settings_manager::{
    DefaultProjectTrust, DoubleEscapeAction, MermaidRenderingMode, TransportSetting,
    TreeFilterMode, WarningSettings,
};
use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_display_text;

/// `SETTINGS_SUBMENU_SELECT_LIST_LAYOUT` (settings-selector.ts:27-30).
const SETTINGS_SUBMENU_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
    truncate_primary: None,
};

/// `DEFAULT_PROJECT_TRUST_LABELS` (settings-selector.ts:42-46).
const DEFAULT_PROJECT_TRUST_LABELS: [(DefaultProjectTrust, &str); 3] = [
    (DefaultProjectTrust::Ask, "Ask"),
    (DefaultProjectTrust::Always, "Always trust"),
    (DefaultProjectTrust::Never, "Never trust"),
];

/// `HTTP_IDLE_TIMEOUT_CHOICES` (http-dispatcher.ts:6-12).
const HTTP_IDLE_TIMEOUT_CHOICES: [(&str, u64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

/// `AUTOMATIC_THEME_VALUE` (settings-selector.ts:230).
const AUTOMATIC_THEME_VALUE: &str = "/";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run a submenu `done` callback at most once (the `FnOnce` is shared by
/// the select and cancel paths of a submenu).
fn call_done(done: &Arc<Mutex<Option<SubmenuDone>>>, value: Option<String>) {
    let mut slot = lock(done);
    if let Some(f) = slot.take() {
        f(value);
    }
}

/// `SettingsConfig` (settings-selector.ts:52-83).
#[derive(Debug, Clone)]
pub struct SettingsSelectorOptions {
    pub auto_compact: bool,
    pub show_images: bool,
    pub image_width_cells: u64,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub enable_skill_commands: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub transport: TransportSetting,
    pub http_idle_timeout_ms: u64,
    /// Global default thinking level (the per-model "(clear override)"
    /// description; the plain "Default thinking level" entry was removed
    /// upstream, 5b3caaf4c).
    pub thinking_level: ThinkingLevel,
    /// Per-model thinking overrides snapshot (settings-selector.ts:53-54).
    pub model_thinking_levels: std::collections::BTreeMap<String, rpi_agent::types::ThinkingLevel>,
    /// `availableDefaultModels` (settings-selector.ts:53): catalog snapshot
    /// for the per-model picker.
    pub available_default_models: Vec<rpi_ai::types::Model>,
    /// `currentModel` — pinned first in the picker.
    pub current_model: Option<rpi_ai::types::Model>,
    /// `defaultModel` ("provider/id" or "not set") — pinned second.
    pub default_model: String,
    pub current_theme: String,
    /// Upstream `TerminalTheme` (`"dark" | "light"`).
    pub terminal_theme: TerminalColorScheme,
    pub available_themes: Vec<String>,
    pub hide_thinking_block: bool,
    pub mermaid_rendering_mode: MermaidRenderingMode,
    pub show_cache_miss_notices: bool,
    pub collapse_changelog: bool,
    pub enable_install_telemetry: bool,
    pub double_escape_action: DoubleEscapeAction,
    pub tree_filter_mode: TreeFilterMode,
    pub show_hardware_cursor: bool,
    pub editor_padding_x: u64,
    pub output_pad: u8,
    pub autocomplete_max_visible: u64,
    pub quiet_startup: bool,
    pub default_project_trust: DefaultProjectTrust,
    pub clear_on_shrink: bool,
    pub show_terminal_progress: bool,
    pub tui_mode: rpi_tui::tui::TuiMode,
    pub fullscreen_exit_output: crate::core::settings_manager::FullscreenExitOutput,
    pub fullscreen_scrollbar: rpi_tui::components::scroll_view::ScrollbarMode,
    pub fullscreen_copy_on_select: bool,
    pub warnings: WarningSettings,
}

/// One event per upstream `SettingsCallbacks.on*Change` (settings-selector.ts:
/// 85-115). [`SettingsChange::ThemePreview`] carries the optional upstream
/// `onThemePreview` hook (see module header).
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsChange {
    AutoCompact(bool),
    ShowImages(bool),
    ImageWidthCells(u64),
    AutoResizeImages(bool),
    BlockImages(bool),
    EnableSkillCommands(bool),
    SteeringMode(QueueMode),
    FollowUpMode(QueueMode),
    Transport(TransportSetting),
    HttpIdleTimeoutMs(u64),
    /// `onModelThinkingLevelChange` (settings-selector.ts:102).
    ModelThinkingLevelChange {
        provider: String,
        model_id: String,
        level: rpi_agent::types::ThinkingLevel,
    },
    /// `onModelThinkingLevelRemove` (settings-selector.ts:103).
    ModelThinkingLevelRemove {
        provider: String,
        model_id: String,
    },
    Theme(String),
    ThemePreview(String),
    HideThinkingBlock(bool),
    MermaidRenderingMode(MermaidRenderingMode),
    ShowCacheMissNotices(bool),
    CollapseChangelog(bool),
    EnableInstallTelemetry(bool),
    DoubleEscapeAction(DoubleEscapeAction),
    TreeFilterMode(TreeFilterMode),
    ShowHardwareCursor(bool),
    EditorPaddingX(u64),
    OutputPad(u8),
    AutocompleteMaxVisible(u64),
    QuietStartup(bool),
    DefaultProjectTrust(DefaultProjectTrust),
    ClearOnShrink(bool),
    ShowTerminalProgress(bool),
    TuiMode(rpi_tui::tui::TuiMode),
    FullscreenExitOutput(crate::core::settings_manager::FullscreenExitOutput),
    FullscreenScrollbar(rpi_tui::components::scroll_view::ScrollbarMode),
    /// `onFullscreenCopyOnSelectChange` (settings-selector.ts:124 @ 9841914,
    /// 4e4949299).
    FullscreenCopyOnSelect(bool),
    Warnings(WarningSettings),
}

// ---------------------------------------------------------------------------
// Value <-> string conversions for the settings choices
// ---------------------------------------------------------------------------

fn queue_mode_to_str(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

fn parse_queue_mode(value: &str) -> QueueMode {
    if value == "all" {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

/// `newValue as MermaidRenderingMode` (settings-selector.ts:822): the
/// selector only offers the three known values; anything unrecognized
/// resolves to `"streaming"` like the settings getter
/// (settings-manager.ts:1264-1265).
fn parse_mermaid_rendering_mode(value: &str) -> MermaidRenderingMode {
    match value {
        "off" => MermaidRenderingMode::Off,
        "final" => MermaidRenderingMode::Final,
        _ => MermaidRenderingMode::Streaming,
    }
}

fn transport_to_str(transport: TransportSetting) -> &'static str {
    match transport {
        Transport::Sse => "sse",
        Transport::Websocket => "websocket",
        Transport::WebsocketCached => "websocket-cached",
        Transport::Auto => "auto",
    }
}

fn parse_transport(value: &str) -> TransportSetting {
    match value {
        "sse" => Transport::Sse,
        "websocket" => Transport::Websocket,
        "websocket-cached" => Transport::WebsocketCached,
        _ => Transport::Auto,
    }
}

fn double_escape_to_str(action: DoubleEscapeAction) -> &'static str {
    match action {
        DoubleEscapeAction::Tree => "tree",
        DoubleEscapeAction::Fork => "fork",
        DoubleEscapeAction::None => "none",
    }
}

fn parse_double_escape(value: &str) -> DoubleEscapeAction {
    match value {
        "tree" => DoubleEscapeAction::Tree,
        "fork" => DoubleEscapeAction::Fork,
        _ => DoubleEscapeAction::None,
    }
}

/// `newValue as TuiMode` (settings-selector.ts:869 @ 5446cd754).
fn tui_mode_to_str(mode: rpi_tui::tui::TuiMode) -> &'static str {
    match mode {
        rpi_tui::tui::TuiMode::Regular => "regular",
        rpi_tui::tui::TuiMode::Fullscreen => "fullscreen",
    }
}

fn parse_tui_mode(value: &str) -> rpi_tui::tui::TuiMode {
    if value == "fullscreen" {
        rpi_tui::tui::TuiMode::Fullscreen
    } else {
        rpi_tui::tui::TuiMode::Regular
    }
}

/// FullscreenExitOutput ↔ "transcript" | "resume-hint"
/// (settings-manager.ts:1140-1142 @ 5446cd754).
fn fullscreen_exit_output_to_str(
    output: crate::core::settings_manager::FullscreenExitOutput,
) -> &'static str {
    match output {
        crate::core::settings_manager::FullscreenExitOutput::Transcript => "transcript",
        crate::core::settings_manager::FullscreenExitOutput::ResumeHint => "resume-hint",
    }
}

fn parse_fullscreen_exit_output(
    value: &str,
) -> crate::core::settings_manager::FullscreenExitOutput {
    if value == "resume-hint" {
        crate::core::settings_manager::FullscreenExitOutput::ResumeHint
    } else {
        crate::core::settings_manager::FullscreenExitOutput::Transcript
    }
}

/// FullscreenScrollbar ↔ "auto" | "always" | "hidden"
/// (settings-manager.ts:1150-1152 @ 6129a353b).
fn fullscreen_scrollbar_to_str(
    mode: rpi_tui::components::scroll_view::ScrollbarMode,
) -> &'static str {
    match mode {
        rpi_tui::components::scroll_view::ScrollbarMode::Auto => "auto",
        rpi_tui::components::scroll_view::ScrollbarMode::Always => "always",
        rpi_tui::components::scroll_view::ScrollbarMode::Hidden => "hidden",
    }
}

fn parse_fullscreen_scrollbar(value: &str) -> rpi_tui::components::scroll_view::ScrollbarMode {
    match value {
        "always" => rpi_tui::components::scroll_view::ScrollbarMode::Always,
        "hidden" => rpi_tui::components::scroll_view::ScrollbarMode::Hidden,
        _ => rpi_tui::components::scroll_view::ScrollbarMode::Auto,
    }
}

fn tree_filter_to_str(mode: TreeFilterMode) -> &'static str {
    match mode {
        TreeFilterMode::Default => "default",
        TreeFilterMode::NoTools => "no-tools",
        TreeFilterMode::UserOnly => "user-only",
        TreeFilterMode::LabeledOnly => "labeled-only",
        TreeFilterMode::All => "all",
    }
}

fn parse_tree_filter(value: &str) -> TreeFilterMode {
    match value {
        "no-tools" => TreeFilterMode::NoTools,
        "user-only" => TreeFilterMode::UserOnly,
        "labeled-only" => TreeFilterMode::LabeledOnly,
        "all" => TreeFilterMode::All,
        _ => TreeFilterMode::Default,
    }
}

fn thinking_level_to_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

/// `DEFAULT_PROJECT_TRUST_BY_LABEL` (settings-selector.ts:48-50).
fn trust_from_label(label: &str) -> Option<DefaultProjectTrust> {
    DEFAULT_PROJECT_TRUST_LABELS
        .iter()
        .find(|(_, l)| *l == label)
        .map(|(trust, _)| *trust)
}

fn trust_label(trust: DefaultProjectTrust) -> &'static str {
    DEFAULT_PROJECT_TRUST_LABELS
        .iter()
        .find(|(t, _)| *t == trust)
        .map(|(_, l)| *l)
        .unwrap_or("Ask")
}

/// `formatHttpIdleTimeoutMs` (http-dispatcher.ts:27-31).
fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    match HTTP_IDLE_TIMEOUT_CHOICES
        .iter()
        .find(|(_, timeout)| *timeout == timeout_ms)
    {
        Some((label, _)) => (*label).to_string(),
        None => format!("{} sec", timeout_ms / 1000),
    }
}

// ---------------------------------------------------------------------------
// Theme builders (getSettingsListTheme / getSelectListTheme, theme.ts:1269-
// 1293)
// ---------------------------------------------------------------------------

fn settings_list_theme(theme: Arc<Theme>) -> Arc<SettingsListTheme> {
    Arc::new(SettingsListTheme {
        label: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str, selected: bool| {
                if selected {
                    theme.fg("accent", text)
                } else {
                    text.to_string()
                }
            })
        },
        value: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str, selected: bool| {
                if selected {
                    theme.fg("accent", text)
                } else {
                    theme.fg("muted", text)
                }
            })
        },
        description: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("dim", text))
        },
        cursor: theme.fg("accent", "→ "),
        hint: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("dim", text))
        },
    })
}

fn select_list_theme(theme: Arc<Theme>) -> Arc<SelectListTheme> {
    Arc::new(SelectListTheme {
        selected_prefix: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("accent", text))
        },
        selected_text: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("accent", text))
        },
        description: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("muted", text))
        },
        scroll_info: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("muted", text))
        },
        no_match: {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("muted", text))
        },
    })
}

// ---------------------------------------------------------------------------
// Warning settings submenu (settings-selector.ts:117-160)
// ---------------------------------------------------------------------------

/// `WarningSettingsSubmenu` (settings-selector.ts:117-160).
struct WarningSettingsSubmenu {
    settings_list: SettingsList,
}

impl WarningSettingsSubmenu {
    fn new(
        warnings: WarningSettings,
        theme: Arc<SettingsListTheme>,
        on_change: Box<dyn FnMut(WarningSettings) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        let mut state = warnings;
        let items = vec![SettingItem {
            id: "anthropic-extra-usage".to_string(),
            label: "Anthropic extra usage".to_string(),
            description: Some(
                "Warn when Anthropic subscription auth may use paid extra usage".to_string(),
            ),
            current_value: if state.anthropic_extra_usage.unwrap_or(true) {
                "true"
            } else {
                "false"
            }
            .to_string(),
            values: Some(vec!["true".to_string(), "false".to_string()]),
            submenu: None,
        }];
        let mut settings_list = SettingsList::new(items, 1, theme, None);
        let mut on_change = Some(on_change);
        settings_list.on_change = Some(Box::new(move |id, new_value| {
            if id == "anthropic-extra-usage" {
                state.anthropic_extra_usage = Some(new_value == "true");
                if let Some(on_change) = on_change.as_mut() {
                    on_change(state.clone());
                }
            }
        }));
        settings_list.on_cancel = Some(on_cancel);
        Self { settings_list }
    }
}

impl Component for WarningSettingsSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        self.settings_list.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.settings_list.handle_input(data);
    }
}

// ---------------------------------------------------------------------------
// Generic select submenu (settings-selector.ts:162-224)
// ---------------------------------------------------------------------------

/// `SelectSubmenuOptions` (settings-submenu.ts:20-23 @ ee29aa118).
#[derive(Default)]
struct SelectSubmenuOptions {
    /// Enable type-to-search fuzzy filtering.
    searchable: bool,
    /// Override the select list layout (column widths).
    layout: Option<SelectListLayoutOptions>,
}

/// Shared select-callback slot: every list rebuild re-attaches thin
/// forwarders (`buildSelectList`, settings-submenu.ts:108-121).
type SharedSelectItemFn = Arc<Mutex<Option<SelectItemFn>>>;
type SharedCancelFn = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

/// `SelectSubmenu` (settings-submenu.ts:26-141 @ ee29aa118): title +
/// optional description + optional search input + `SelectList` + hint.
struct SelectSubmenu {
    title: String,
    description: String,
    select_list: SelectList,
    theme: Arc<Theme>,
    layout: SelectListLayoutOptions,
    /// Search input (searchable mode only).
    search: Option<rpi_tui::components::input::Input>,
    all_options: Vec<SelectItem>,
    on_select: SharedSelectItemFn,
    on_cancel: SharedCancelFn,
}

impl SelectSubmenu {
    #[allow(clippy::too_many_arguments)]
    fn new(
        title: &str,
        description: &str,
        options: Vec<SelectItem>,
        current_value: &str,
        theme: Arc<Theme>,
        select_list_theme: Arc<SelectListTheme>,
        on_select: Option<SelectItemFn>,
        on_cancel: Option<Box<dyn FnMut() + Send>>,
        on_selection_change: Option<SelectItemFn>,
        submenu_options: Option<SelectSubmenuOptions>,
    ) -> Self {
        let submenu_options = submenu_options.unwrap_or_default();
        let layout = submenu_options
            .layout
            .unwrap_or(SETTINGS_SUBMENU_SELECT_LIST_LAYOUT);
        let on_select: SharedSelectItemFn = Arc::new(Mutex::new(on_select));
        let on_cancel: SharedCancelFn = Arc::new(Mutex::new(on_cancel));
        let on_selection_change: SharedSelectItemFn = Arc::new(Mutex::new(on_selection_change));
        let select_list = Self::build_select_list(
            &options,
            current_value,
            &select_list_theme,
            layout.clone(),
            Arc::clone(&on_select),
            Arc::clone(&on_cancel),
            on_selection_change,
        );
        let search = if submenu_options.searchable {
            let mut input = rpi_tui::components::input::Input::new();
            use rpi_tui::tui::Focusable as _;
            input.set_focused(false);
            Some(input)
        } else {
            None
        };
        Self {
            title: title.to_string(),
            description: description.to_string(),
            select_list,
            theme,
            layout,
            search,
            all_options: options,
            on_select,
            on_cancel,
        }
    }

    /// `buildSelectList` (settings-submenu.ts:108-121): fresh list with
    /// forwarder callbacks so rebuilds keep the wiring.
    #[allow(clippy::too_many_arguments)]
    fn build_select_list(
        options: &[SelectItem],
        preselect: &str,
        select_list_theme: &Arc<SelectListTheme>,
        layout: SelectListLayoutOptions,
        on_select: SharedSelectItemFn,
        on_cancel: SharedCancelFn,
        on_selection_change: SharedSelectItemFn,
    ) -> SelectList {
        let mut list = SelectList::new(
            options.to_vec(),
            options.len().min(10),
            Arc::clone(select_list_theme),
            Some(layout),
        );
        if let Some(index) = options.iter().position(|o| o.value == preselect) {
            list.set_selected_index(index);
        }
        list.on_select = Some(Box::new(move |item: &SelectItem| {
            if let Ok(mut callback) = on_select.lock() {
                if let Some(callback) = callback.as_mut() {
                    callback(item);
                }
            }
        }));
        list.on_cancel = Some(Box::new(move || {
            if let Ok(mut callback) = on_cancel.lock() {
                if let Some(callback) = callback.as_mut() {
                    callback();
                }
            }
        }));
        list.on_selection_change = Some(Box::new(move |item: &SelectItem| {
            if let Ok(mut callback) = on_selection_change.lock() {
                if let Some(callback) = callback.as_mut() {
                    callback(item);
                }
            }
        }));
        list
    }

    /// `applyFilter` (settings-submenu.ts:124-131): rebuild the list from
    /// the fuzzy-filtered options (label + description text).
    fn apply_filter(&mut self, select_list_theme: &Arc<SelectListTheme>, query: &str) {
        let filtered: Vec<SelectItem> = if query.is_empty() {
            self.all_options.clone()
        } else {
            rpi_tui::fuzzy::fuzzy_filter(self.all_options.clone(), query, |item: &SelectItem| {
                format!(
                    "{} {}",
                    item.label,
                    item.description.clone().unwrap_or_default()
                )
            })
        };
        let selected_value = self
            .select_list
            .get_selected_item()
            .map(|item| item.value.clone())
            .unwrap_or_default();
        self.select_list = Self::build_select_list(
            &filtered,
            &selected_value,
            select_list_theme,
            self.layout.clone(),
            Arc::clone(&self.on_select),
            Arc::clone(&self.on_cancel),
            // Upstream `buildSelectList` re-attaches onSelectionChange; a
            // rebuild during filtering triggers no selection event, so a
            // no-op forwarder is equivalent here.
            Arc::new(Mutex::new(None)),
        );
    }
}

impl Component for SelectSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(Theme::bold(&self.theme.fg("accent", &self.title)));
        if !self.description.is_empty() {
            lines.push(String::new());
            lines.push(self.theme.fg("muted", &self.description));
        }
        if let Some(search_input) = &self.search {
            lines.push(String::new());
            lines.extend(search_input.render(width));
        }
        lines.push(String::new());
        lines.extend(self.select_list.render(width));
        lines.push(String::new());
        let hint = if self.search.is_some() {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        lines.push(self.theme.fg("dim", hint));
        lines
    }

    /// `handleInput` (settings-submenu.ts:133-141): navigation keys reach
    /// the list; everything else feeds the search input and refilters.
    fn handle_input(&mut self, data: &str) {
        if self.search.is_some() {
            let keybindings = rpi_tui::keybindings::get_keybindings();
            let read = keybindings.read().unwrap_or_else(|e| e.into_inner());
            let is_nav = read.matches_id(data, "tui.select.up")
                || read.matches_id(data, "tui.select.down")
                || read.matches_id(data, "tui.select.confirm")
                || read.matches_id(data, "tui.select.cancel");
            drop(read);
            if is_nav {
                self.select_list.handle_input(data);
                return;
            }
            let select_list_theme = select_list_theme(Arc::clone(&self.theme));
            let Some(search_input) = self.search.as_mut() else {
                return;
            };
            search_input.handle_input(data);
            let query = search_input.get_value().to_string();
            self.apply_filter(&select_list_theme, &query);
            return;
        }
        self.select_list.handle_input(data);
    }
}

// ---------------------------------------------------------------------------
// Per-model thinking submenu (settings-selector.ts:577-662 @ 2ff8ba622 +
// ee29aa118 + a669db3c3 + f2a622789; SteppedSubmenu shape,
// settings-submenu.ts:148-235)
// ---------------------------------------------------------------------------

/// `CLEAR_OVERRIDE_VALUE` (settings-selector.ts:41).
const CLEAR_OVERRIDE_VALUE: &str = "__clear__";

/// `MODEL_PICKER_LAYOUT` (settings-selector.ts:30, a669db3c3).
const MODEL_PICKER_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(46),
    truncate_primary: None,
};

/// `modelSettingKey` (settings-selector.ts:176-178).
fn model_setting_key(model: &rpi_ai::types::Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// `modelDisplayLabel` (settings-selector.ts:180-182 @ a669db3c3): plain
/// `modelid [provider]` for titles.
fn model_display_label(model: &rpi_ai::types::Model) -> String {
    format!("{} [{}]", model.id, model.provider)
}

/// `modelItemLabel` (settings-selector.ts:190-192 @ a669db3c3): list rows
/// use a muted provider badge like /model.
fn model_item_label(model: &rpi_ai::types::Model, theme: &Theme) -> String {
    format!(
        "{} {}",
        model.id,
        theme.fg("muted", &format!("[{}]", model.provider))
    )
}

/// `modelThinkingOverridesSummary` (settings-selector.ts:199-201): the
/// "model-thinking" item current value. Upstream shows the count
/// (`{count} configured`); the value is computed fresh at display time.
fn model_thinking_overrides_summary(
    overrides: &std::collections::BTreeMap<String, rpi_agent::types::ThinkingLevel>,
) -> String {
    if overrides.is_empty() {
        "not set".to_string()
    } else {
        format!("{} configured", overrides.len())
    }
}

/// Description per agent-side thinking level value ("off" included) —
/// `THINKING_DESCRIPTIONS` (settings-selector.ts:29-40).
fn model_thinking_description(level: &str) -> &'static str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning (~1k tokens)",
        "low" => "Light reasoning (~2k tokens)",
        "medium" => "Moderate reasoning (~8k tokens)",
        "high" => "Deep reasoning (~16k tokens)",
        "xhigh" => "Extra-high reasoning (~32k tokens)",
        _ => "Maximum reasoning",
    }
}

/// Agent-side thinking level value string (`rpi_agent::types::ThinkingLevel`
/// = `rpi_ai::ModelThinkingLevel`, off included).
fn agent_level_str(level: &rpi_agent::types::ThinkingLevel) -> &'static str {
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

/// Agent-side thinking level from a value string.
fn agent_level_from_str(value: &str) -> Option<rpi_agent::types::ThinkingLevel> {
    match value {
        "off" => Some(rpi_agent::types::ThinkingLevel::Off),
        "minimal" => Some(rpi_agent::types::ThinkingLevel::Minimal),
        "low" => Some(rpi_agent::types::ThinkingLevel::Low),
        "medium" => Some(rpi_agent::types::ThinkingLevel::Medium),
        "high" => Some(rpi_agent::types::ThinkingLevel::High),
        "xhigh" => Some(rpi_agent::types::ThinkingLevel::Xhigh),
        "max" => Some(rpi_agent::types::ThinkingLevel::Max),
        _ => None,
    }
}

/// Callback: apply a per-model thinking override
/// (`onModelThinkingLevelChange`, settings-selector.ts:102).
type ModelThinkingChangeFn = Box<dyn FnMut(&str, &str, rpi_agent::types::ThinkingLevel) + Send>;
/// Callback: remove a per-model thinking override
/// (`onModelThinkingLevelRemove`, settings-selector.ts:103).
type ModelThinkingRemoveFn = Box<dyn FnMut(&str, &str) + Send>;

/// The model-thinking two-step submenu (upstream `SteppedSubmenu` with
/// `loop: true`): step 1 = searchable model picker; step 2 = level picker
/// with ✓ markers and a "(clear override)" row. Esc goes back a step (Esc
/// at step 0 cancels); a level selection applies the override and loops
/// back to step 0 (settings-submenu.ts:148-235).
struct ModelThinkingSubmenu {
    inner: Arc<Mutex<ModelThinkingInner>>,
}

struct ModelThinkingInner {
    available_models: Vec<rpi_ai::types::Model>,
    current_model: Option<rpi_ai::types::Model>,
    default_model: String,
    /// Shared with the settings item (constructor-local mutable copy,
    /// settings-selector.ts:463): updates survive submenu re-entry and
    /// feed the summary written back to the item.
    overrides: Arc<Mutex<std::collections::BTreeMap<String, rpi_agent::types::ThinkingLevel>>>,
    /// Global default (for the clear-override description).
    thinking_level: ThinkingLevel,
    theme: Arc<Theme>,
    select_list_theme: Arc<SelectListTheme>,
    on_change: Option<ModelThinkingChangeFn>,
    on_remove: Option<ModelThinkingRemoveFn>,
    on_done: Option<Box<dyn FnMut() + Send>>,
    step: usize,
    context_model: Option<String>,
    component: Option<Box<dyn rpi_tui::tui::Component + Send>>,
}

impl ModelThinkingSubmenu {
    #[allow(clippy::too_many_arguments)] // mirrors the upstream SteppedSubmenu wiring
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn new(
        available_models: Vec<rpi_ai::types::Model>,
        current_model: Option<rpi_ai::types::Model>,
        default_model: String,
        overrides: Arc<Mutex<std::collections::BTreeMap<String, rpi_agent::types::ThinkingLevel>>>,
        thinking_level: ThinkingLevel,
        theme: Arc<Theme>,
        select_list_theme: Arc<SelectListTheme>,
        on_change: ModelThinkingChangeFn,
        on_remove: ModelThinkingRemoveFn,
        on_done: Box<dyn FnMut() + Send>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(ModelThinkingInner {
            available_models,
            current_model,
            default_model,
            overrides,
            thinking_level,
            theme,
            select_list_theme,
            on_change: Some(on_change),
            on_remove: Some(on_remove),
            on_done: Some(on_done),
            step: 0,
            context_model: None,
            component: None,
        }));
        build_model_step(&inner);
        Self { inner }
    }
}

/// `buildStep(0)` — the model picker (settings-selector.ts:583-617):
/// current model first, then the persisted default, then provider order;
/// `modelid [provider]` labels (a669db3c3), override values as
/// descriptions; searchable (ee29aa118); "Step 1/2 · " title prefix.
fn build_model_step(inner: &Arc<Mutex<ModelThinkingInner>>) {
    let (items, preselect, theme, select_list_theme) = {
        let state = lock(inner);
        let current_model_key = state
            .current_model
            .as_ref()
            .map(model_setting_key)
            .unwrap_or_default();
        let default_model_key = state.default_model.clone();
        let mut sorted = state.available_models.clone();
        sorted.sort_by(|a, b| {
            let a_key = model_setting_key(a);
            let b_key = model_setting_key(b);
            if a_key == current_model_key {
                std::cmp::Ordering::Less
            } else if b_key == current_model_key {
                std::cmp::Ordering::Greater
            } else if a_key == default_model_key {
                std::cmp::Ordering::Less
            } else if b_key == default_model_key {
                std::cmp::Ordering::Greater
            } else {
                a.provider.cmp(&b.provider)
            }
        });
        let overrides = lock(&state.overrides).clone();
        let mut items: Vec<SelectItem> = sorted
            .iter()
            .map(|model| {
                let key = model_setting_key(model);
                SelectItem {
                    description: overrides
                        .get(&key)
                        .map(|level| agent_level_str(level).to_string()),
                    label: model_item_label(model, &state.theme),
                    value: key,
                }
            })
            .collect();
        if items.is_empty() {
            items.push(SelectItem {
                value: "__none__".to_string(),
                label: "No models available".to_string(),
                description: Some("Log in to a provider or configure an API key first".to_string()),
            });
        }
        let preselect = if !current_model_key.is_empty() {
            current_model_key
        } else {
            default_model_key
        };
        (
            items,
            preselect,
            Arc::clone(&state.theme),
            Arc::clone(&state.select_list_theme),
        )
    };
    lock(inner).step = 0;

    let select_inner = Arc::clone(inner);
    let on_select: SelectItemFn = Box::new(move |item: &SelectItem| {
        // Step 1 → step 2 (SteppedSubmenu advances on select).
        let mut state = lock(&select_inner);
        state.context_model = Some(item.value.clone());
        drop(state);
        build_level_step(&select_inner);
    });
    let cancel_inner = Arc::clone(inner);
    let on_cancel: Box<dyn FnMut() + Send> = Box::new(move || model_thinking_done(&cancel_inner));

    let menu = SelectSubmenu::new(
        "Per-Model Thinking Level",
        "Step 1/2 · Select a model to configure",
        items,
        &preselect,
        theme,
        select_list_theme,
        Some(on_select),
        Some(on_cancel),
        None,
        Some(SelectSubmenuOptions {
            searchable: true,
            layout: Some(MODEL_PICKER_LAYOUT),
        }),
    );
    lock(inner).component = Some(Box::new(menu));
}

/// `buildStep(1)` — the level picker (settings-selector.ts:619-646):
/// supported levels (or `off` for non-reasoning models) with `✓ ` markers
/// on the active override (f2a622789) + "(clear override)" when one exists.
fn build_level_step(inner: &Arc<Mutex<ModelThinkingInner>>) {
    let (title, items, preselect, theme, select_list_theme) = {
        let state = lock(inner);
        let key = state.context_model.clone().unwrap_or_default();
        let model = state
            .available_models
            .iter()
            .find(|model| model_setting_key(model) == key);
        let title = match model {
            Some(model) => format!("Thinking Level for {}", model_display_label(model)),
            None => format!("Thinking Level for {key}"),
        };
        let mut items: Vec<SelectItem> = Vec::new();
        let mut preselect = String::new();
        if let Some(model) = model {
            let overrides = lock(&state.overrides);
            let active = overrides.get(&key).copied();
            let levels: Vec<rpi_agent::types::ThinkingLevel> = if model.reasoning {
                rpi_ai::models::get_supported_thinking_levels(model)
            } else {
                vec![rpi_agent::types::ThinkingLevel::Off]
            };
            for level in levels {
                let value = agent_level_str(&level);
                if active == Some(level) {
                    preselect = value.to_string();
                }
                items.push(SelectItem {
                    value: value.to_string(),
                    label: format!(
                        "{}{}",
                        if active == Some(level) {
                            "\u{2713} "
                        } else {
                            "  "
                        },
                        value
                    ),
                    description: Some(model_thinking_description(value).to_string()),
                });
            }
            if active.is_some() {
                items.push(SelectItem {
                    value: CLEAR_OVERRIDE_VALUE.to_string(),
                    label: "  (clear override)".to_string(),
                    description: Some(format!(
                        "Revert to global default ({})",
                        thinking_level_to_str(state.thinking_level)
                    )),
                });
            }
        }
        (
            title,
            items,
            preselect,
            Arc::clone(&state.theme),
            Arc::clone(&state.select_list_theme),
        )
    };
    lock(inner).step = 1;

    let select_inner = Arc::clone(inner);
    let on_select: SelectItemFn = Box::new(move |item: &SelectItem| {
        // Final step: deliver the result, then loop back to step 0
        // (SteppedSubmenu `{ loop: true }`).
        let mut state = lock(&select_inner);
        let Some(key) = state.context_model.clone() else {
            return;
        };
        let Some((provider, model_id)) = key.split_once('/') else {
            return;
        };
        if item.value == CLEAR_OVERRIDE_VALUE {
            lock(&state.overrides).remove(&key);
            if let Some(on_remove) = state.on_remove.as_mut() {
                on_remove(provider, model_id);
            }
        } else if let Some(level) = agent_level_from_str(&item.value) {
            lock(&state.overrides).insert(key.clone(), level);
            if let Some(on_change) = state.on_change.as_mut() {
                on_change(provider, model_id, level);
            }
        }
        state.context_model = None;
        drop(state);
        build_model_step(&select_inner);
    });
    let cancel_inner = Arc::clone(inner);
    // Esc at step > 0 goes back one step (SteppedSubmenu onCancel).
    let on_cancel: Box<dyn FnMut() + Send> = Box::new(move || {
        build_model_step(&cancel_inner);
    });

    let menu = SelectSubmenu::new(
        &title,
        "Step 2/2 · Select default thinking level for this model",
        items,
        &preselect,
        theme,
        select_list_theme,
        Some(on_select),
        Some(on_cancel),
        None,
        None,
    );
    lock(inner).component = Some(Box::new(menu));
}

/// The SteppedSubmenu top-level cancel (Esc at step 0 / final done):
/// writes the fresh summary back to the item.
fn model_thinking_done(inner: &Arc<Mutex<ModelThinkingInner>>) {
    let mut state = lock(inner);
    if let Some(mut on_done) = state.on_done.take() {
        on_done();
    }
}

impl rpi_tui::tui::Component for ModelThinkingSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        match &lock(&self.inner).component {
            Some(component) => component.render(width),
            None => Vec::new(),
        }
    }

    fn handle_input(&mut self, data: &str) {
        // Take the component out so a step transition inside `handle_input`
        // can replace it (ThemeSubmenu pattern; upstream swaps the active
        // child mid-dispatch).
        let component = lock(&self.inner).component.take();
        if let Some(mut component) = component {
            component.handle_input(data);
            let mut inner = lock(&self.inner);
            if inner.component.is_none() {
                inner.component = Some(component);
            }
        }
    }

    fn invalidate(&mut self) {
        if let Some(component) = lock(&self.inner).component.as_mut() {
            component.invalidate();
        }
    }
}

// ---------------------------------------------------------------------------
// Theme submenu (settings-selector.ts:226-467)
// ---------------------------------------------------------------------------

/// `themeItems` (settings-selector.ts:194-199 @ 3fc3ef532): the CURRENT
/// (saved) theme keeps a `✓ ` prefix while the preview cursor browses —
/// the marker distinguishes the saved value from the preview.
fn theme_items(available_themes: &[String], current_theme: &str) -> Vec<SelectItem> {
    available_themes
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: format!(
                "{}{}",
                if name == current_theme {
                    "\u{2713} "
                } else {
                    "  "
                },
                name
            ),
            description: None,
        })
        .collect()
}

/// `singleModeThemeItems` (settings-selector.ts:202-212 @ 3fc3ef532).
fn single_mode_theme_items(available_themes: &[String], current_theme: &str) -> Vec<SelectItem> {
    let mut items = vec![SelectItem {
        value: AUTOMATIC_THEME_VALUE.to_string(),
        label: "  Automatic".to_string(),
        description: Some("Use separate themes for light and dark terminal appearance".to_string()),
    }];
    items.extend(theme_items(available_themes, current_theme));
    items
}

/// `preferredTheme` (settings-selector.ts:243-247).
fn preferred_theme(available_themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    if let Some(preferred) = preferred {
        if available_themes.iter().any(|t| t == preferred) {
            return preferred.to_string();
        }
    }
    if available_themes.iter().any(|t| t == fallback) {
        return fallback.to_string();
    }
    available_themes
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// `defaultAutomaticThemes` (settings-selector.ts:249-259).
fn default_automatic_themes(
    current_theme_setting: &str,
    available_themes: &[String],
) -> (String, String) {
    if let Some((light, dark)) = parse_auto_theme_setting(Some(current_theme_setting)) {
        return (light, dark);
    }
    let current_fixed_theme = if current_theme_setting.contains('/') {
        None
    } else {
        Some(current_theme_setting)
    };
    let theme_name = preferred_theme(available_themes, current_fixed_theme, "dark");
    (theme_name.clone(), theme_name)
}

/// `parseAutoThemeSetting` (theme.ts:648-662).
fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<(String, String)> {
    let theme_setting = theme_setting?;
    let mut slashes = theme_setting.match_indices('/');
    let first = slashes.next()?;
    if slashes.next().is_some() {
        return None;
    }
    let (slash_index, _) = first;
    let light = theme_setting[..slash_index].trim();
    let dark = theme_setting[slash_index + 1..].trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_string(), dark.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Single,
    Automatic,
}

/// Shared state of [`ThemeSubmenu`] (upstream fields + the swapped child
/// tree; see module header).
struct ThemeSubmenuInner {
    mode: ThemeMode,
    single_theme: String,
    light_theme: String,
    dark_theme: String,
    terminal_theme: TerminalColorScheme,
    available_themes: Arc<Vec<String>>,
    theme: Arc<Theme>,
    settings_list_theme: Arc<SettingsListTheme>,
    select_list_theme: Arc<SelectListTheme>,
    original_theme_setting: String,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_theme_preview: Option<Box<dyn FnMut(&str) + Send>>,
    on_done: Option<SubmenuDone>,
    /// The current child UI (upstream `inputComponent`; for the automatic
    /// menu it is a labeled wrapper over the settings list, so one slot
    /// serves render and input — see module header).
    component: Option<Box<dyn Component>>,
}

impl ThemeSubmenuInner {
    fn theme_setting(&self) -> String {
        if self.mode == ThemeMode::Automatic {
            self.automatic_theme_setting()
        } else {
            self.single_theme.clone()
        }
    }

    fn active_automatic_theme(&self) -> String {
        if self.terminal_theme == TerminalColorScheme::Light {
            self.light_theme.clone()
        } else {
            self.dark_theme.clone()
        }
    }

    fn automatic_theme_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }
}

/// `ThemeSubmenu` (settings-selector.ts:261-467).
struct ThemeSubmenu {
    inner: Arc<Mutex<ThemeSubmenuInner>>,
}

impl ThemeSubmenu {
    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn new(
        current_theme_setting: &str,
        terminal_theme: TerminalColorScheme,
        available_themes: Arc<Vec<String>>,
        theme: Arc<Theme>,
        settings_list_theme: Arc<SettingsListTheme>,
        select_list_theme: Arc<SelectListTheme>,
        on_theme_preview: Option<Box<dyn FnMut(&str) + Send>>,
        on_done: SubmenuDone,
    ) -> Self {
        let auto_theme = parse_auto_theme_setting(Some(current_theme_setting));
        let automatic_themes = default_automatic_themes(current_theme_setting, &available_themes);
        let fixed_theme = if auto_theme.is_some() || current_theme_setting.contains('/') {
            None
        } else {
            Some(current_theme_setting.to_string())
        };
        let active_automatic = if terminal_theme == TerminalColorScheme::Light {
            automatic_themes.0.clone()
        } else {
            automatic_themes.1.clone()
        };
        let mode = if auto_theme.is_some() {
            ThemeMode::Automatic
        } else {
            ThemeMode::Single
        };
        let single_theme = preferred_theme(
            &available_themes,
            fixed_theme.as_deref().or_else(|| {
                if auto_theme.is_some() {
                    Some(active_automatic.as_str())
                } else {
                    None
                }
            }),
            "dark",
        );

        let inner = Arc::new(Mutex::new(ThemeSubmenuInner {
            mode,
            single_theme,
            light_theme: automatic_themes.0,
            dark_theme: automatic_themes.1,
            terminal_theme,
            available_themes,
            theme,
            settings_list_theme,
            select_list_theme,
            original_theme_setting: current_theme_setting.to_string(),
            on_theme_preview,
            on_done: Some(on_done),
            component: None,
        }));
        if mode == ThemeMode::Automatic {
            show_automatic_menu(&inner);
        } else {
            show_single_menu(&inner);
        }
        Self { inner }
    }
}

/// `apply` (settings-selector.ts:459-461).
fn apply(inner: &Arc<Mutex<ThemeSubmenuInner>>, value: Option<String>) {
    let mut inner = lock(inner);
    if let Some(done) = inner.on_done.take() {
        done(value);
    }
}

/// `cancel` (settings-selector.ts:463-466).
fn cancel(inner: &Arc<Mutex<ThemeSubmenuInner>>) {
    {
        let mut inner = lock(inner);
        let original = inner.original_theme_setting.clone();
        if let Some(preview) = inner.on_theme_preview.as_mut() {
            preview(&original);
        }
    }
    apply(inner, None);
}

/// `showSingleMenu` (settings-selector.ts:315-339).
fn show_single_menu(inner: &Arc<Mutex<ThemeSubmenuInner>>) {
    let items = {
        let inner = lock(inner);
        single_mode_theme_items(&inner.available_themes, &inner.single_theme)
    };
    let current = lock(inner).single_theme.clone();

    let select_inner = Arc::clone(inner);
    let on_select: SelectItemFn = Box::new(move |item: &SelectItem| {
        let mut state = lock(&select_inner);
        if item.value == AUTOMATIC_THEME_VALUE {
            state.mode = ThemeMode::Automatic;
            let setting = state.theme_setting();
            if let Some(preview) = state.on_theme_preview.as_mut() {
                preview(&setting);
            }
            drop(state);
            show_automatic_menu(&select_inner);
            return;
        }
        state.single_theme = item.value.clone();
        let setting = state.single_theme.clone();
        drop(state);
        apply(&select_inner, Some(setting));
    });
    let cancel_inner = Arc::clone(inner);
    let on_cancel: Box<dyn FnMut() + Send> = Box::new(move || cancel(&cancel_inner));
    let selection_inner = Arc::clone(inner);
    let on_selection_change: SelectItemFn = Box::new(move |item: &SelectItem| {
        let mut state = lock(&selection_inner);
        let setting = if item.value == AUTOMATIC_THEME_VALUE {
            state.automatic_theme_setting()
        } else {
            item.value.clone()
        };
        if let Some(preview) = state.on_theme_preview.as_mut() {
            preview(&setting);
        }
    });

    let (theme, select_list_theme) = {
        let inner = lock(inner);
        (
            Arc::clone(&inner.theme),
            Arc::clone(&inner.select_list_theme),
        )
    };
    let menu = SelectSubmenu::new(
        "Theme",
        "Select a theme, or choose Automatic to follow terminal appearance.",
        items,
        &current,
        theme,
        select_list_theme,
        Some(on_select),
        Some(on_cancel),
        Some(on_selection_change),
        None,
    );
    lock(inner).component = Some(Box::new(menu));
}

/// `showAutomaticMenu` (settings-selector.ts:341-424).
fn show_automatic_menu(inner: &Arc<Mutex<ThemeSubmenuInner>>) {
    let (light_theme, dark_theme, theme, settings_list_theme) = {
        let inner = lock(inner);
        (
            inner.light_theme.clone(),
            inner.dark_theme.clone(),
            Arc::clone(&inner.theme),
            Arc::clone(&inner.settings_list_theme),
        )
    };

    let light_factory = theme_select_factory(
        inner,
        "Light Theme",
        "Select the theme to use for light terminal appearance",
        ThemeTarget::Light,
    );
    let dark_factory = theme_select_factory(
        inner,
        "Dark Theme",
        "Select the theme to use for dark terminal appearance",
        ThemeTarget::Dark,
    );

    let items = vec![
        SettingItem {
            id: "light-theme".to_string(),
            label: "Light theme".to_string(),
            description: Some(
                "Theme to use in automatic mode when the terminal is light".to_string(),
            ),
            current_value: light_theme,
            values: None,
            submenu: Some(light_factory),
        },
        SettingItem {
            id: "dark-theme".to_string(),
            label: "Dark theme".to_string(),
            description: Some(
                "Theme to use in automatic mode when the terminal is dark".to_string(),
            ),
            current_value: dark_theme,
            values: None,
            submenu: Some(dark_factory),
        },
        SettingItem {
            id: "apply".to_string(),
            label: "Apply".to_string(),
            description: Some("Save and go back".to_string()),
            current_value: "save and go back".to_string(),
            values: Some(vec!["save and go back".to_string()]),
            submenu: None,
        },
        SettingItem {
            id: "single-mode".to_string(),
            label: "Change mode".to_string(),
            description: Some("Switch to one theme for light and dark".to_string()),
            current_value: "switch to single theme".to_string(),
            values: Some(vec!["switch to single theme".to_string()]),
            submenu: None,
        },
    ];

    let mut settings_list = SettingsList::new(
        items,
        // `Math.min(items.length, 10)`
        4,
        settings_list_theme,
        None,
    );
    let change_inner = Arc::clone(inner);
    settings_list.on_change = Some(Box::new(move |id, _new_value| match id {
        "single-mode" => {
            let mut state = lock(&change_inner);
            state.mode = ThemeMode::Single;
            state.single_theme = state.active_automatic_theme();
            let single_theme = state.single_theme.clone();
            if let Some(preview) = state.on_theme_preview.as_mut() {
                preview(&single_theme);
            }
            drop(state);
            show_single_menu(&change_inner);
        }
        "apply" => {
            let setting = lock(&change_inner).automatic_theme_setting();
            apply(&change_inner, Some(setting));
        }
        _ => {}
    }));
    let cancel_inner = Arc::clone(inner);
    settings_list.on_cancel = Some(Box::new(move || cancel(&cancel_inner)));

    // Content: title + descriptions + spacer + list (upstream builds a
    // Container for render and passes the list for input; one slot serves
    // both — see module header).
    let header_lines = vec![
        Theme::bold(&theme.fg("accent", "Automatic Theme")),
        String::new(),
        theme.fg(
            "muted",
            "Choose themes for terminal light and dark appearance.",
        ),
        theme.fg("muted", "Light/dark detection requires terminal support."),
        String::new(),
    ];
    lock(inner).component = Some(Box::new(LabeledComponent {
        header_lines,
        inner: Box::new(settings_list),
    }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeTarget {
    Light,
    Dark,
}

/// `createThemeSelect` (settings-selector.ts:426-445) as a `SettingItem`
/// submenu factory for the light/dark theme rows.
fn theme_select_factory(
    inner: &Arc<Mutex<ThemeSubmenuInner>>,
    title: &'static str,
    description: &'static str,
    target: ThemeTarget,
) -> SubmenuFactory {
    let inner = Arc::clone(inner);
    Box::new(move |current_value: &str, done: SubmenuDone| {
        let done = Arc::new(Mutex::new(Some(done)));

        let select_inner = Arc::clone(&inner);
        let on_select: SelectItemFn = Box::new(move |item: &SelectItem| {
            let mut state = lock(&select_inner);
            match target {
                ThemeTarget::Light => state.light_theme = item.value.clone(),
                ThemeTarget::Dark => state.dark_theme = item.value.clone(),
            }
            let setting = state.theme_setting();
            if let Some(preview) = state.on_theme_preview.as_mut() {
                preview(&setting);
            }
            drop(state);
            call_done(&done, Some(setting));
        });
        let cancel_inner = Arc::clone(&inner);
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(move || cancel(&cancel_inner));
        let selection_inner = Arc::clone(&inner);
        let on_selection_change: SelectItemFn = Box::new(move |item: &SelectItem| {
            let mut state = lock(&selection_inner);
            if let Some(preview) = state.on_theme_preview.as_mut() {
                preview(&item.value);
            }
        });

        let (theme, select_list_theme, available_themes, current) = {
            let inner = lock(&inner);
            (
                Arc::clone(&inner.theme),
                Arc::clone(&inner.select_list_theme),
                inner.available_themes.clone(),
                current_value.to_string(),
            )
        };
        let select = SelectSubmenu::new(
            title,
            description,
            theme_items(&available_themes, &current),
            current_value,
            theme,
            select_list_theme,
            Some(on_select),
            Some(on_cancel),
            Some(on_selection_change),
            None,
        );
        Box::new(select)
    })
}

/// A header block rendered above an inner component; input delegates to the
/// inner component (see module header).
struct LabeledComponent {
    header_lines: Vec<String>,
    inner: Box<dyn Component>,
}

impl Component for LabeledComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.header_lines.clone();
        lines.extend(self.inner.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.inner.handle_input(data);
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

impl Component for ThemeSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        match &lock(&self.inner).component {
            Some(component) => component.render(width),
            None => Vec::new(),
        }
    }

    fn handle_input(&mut self, data: &str) {
        // Take the component out so a mode switch inside `handle_input` can
        // replace it (upstream swaps the child tree mid-dispatch).
        let component = lock(&self.inner).component.take();
        if let Some(mut component) = component {
            component.handle_input(data);
            let mut inner = lock(&self.inner);
            if inner.component.is_none() {
                inner.component = Some(component);
            }
        }
    }

    fn invalidate(&mut self) {
        if let Some(component) = lock(&self.inner).component.as_mut() {
            component.invalidate();
        }
    }
}

// ---------------------------------------------------------------------------
// Main settings selector (settings-selector.ts:472-838)
// ---------------------------------------------------------------------------

/// `SettingsSelectorComponent` (settings-selector.ts:472-838).
pub struct SettingsSelectorComponent {
    top_border: DynamicBorder,
    settings_list: SettingsList,
    bottom_border: DynamicBorder,
    focused: bool,
}

impl SettingsSelectorComponent {
    pub fn new(
        options: SettingsSelectorOptions,
        theme: Arc<Theme>,
        on_change: Box<dyn FnMut(SettingsChange) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        let supports_images = get_capabilities().images.is_some();
        let follow_up_key = key_display_text("app.message.followUp");
        let on_change = Arc::new(Mutex::new(on_change));
        let settings_list_theme = settings_list_theme(Arc::clone(&theme));
        let select_list_theme = select_list_theme(Arc::clone(&theme));

        // `currentWarnings` (settings-selector.ts:480): snapshot mutated by
        // the warnings submenu, shared through the submenu factory.
        let current_warnings = Arc::new(Mutex::new(options.warnings.clone()));
        // Shared per-model thinking overrides (constructor-local mutable
        // copy, settings-selector.ts:463): updates survive submenu
        // re-entry and feed the summary written back to the item.
        let current_model_thinking_levels =
            Arc::new(Mutex::new(options.model_thinking_levels.clone()));

        let mut items: Vec<SettingItem> = vec![
            SettingItem {
                id: "autocompact".to_string(),
                label: "Auto-compact".to_string(),
                description: Some(
                    "Automatically compact context when it gets too large".to_string(),
                ),
                current_value: if options.auto_compact { "true" } else { "false" }.to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "steering-mode".to_string(),
                label: "Steering mode".to_string(),
                description: Some(
                    "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.".to_string(),
                ),
                current_value: queue_mode_to_str(options.steering_mode).to_string(),
                values: Some(vec!["one-at-a-time".to_string(), "all".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "follow-up-mode".to_string(),
                label: "Follow-up mode".to_string(),
                description: Some(format!(
                    "{follow_up_key} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
                )),
                current_value: queue_mode_to_str(options.follow_up_mode).to_string(),
                values: Some(vec!["one-at-a-time".to_string(), "all".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "transport".to_string(),
                label: "Transport".to_string(),
                description: Some(
                    "Preferred transport for providers that support multiple transports".to_string(),
                ),
                current_value: transport_to_str(options.transport).to_string(),
                values: Some(vec![
                    "sse".to_string(),
                    "websocket".to_string(),
                    "websocket-cached".to_string(),
                    "auto".to_string(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "http-idle-timeout".to_string(),
                label: "HTTP idle timeout".to_string(),
                description: Some(
                    "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes.".to_string(),
                ),
                current_value: format_http_idle_timeout_ms(options.http_idle_timeout_ms),
                values: Some(
                    HTTP_IDLE_TIMEOUT_CHOICES
                        .iter()
                        .map(|(label, _)| label.to_string())
                        .collect(),
                ),
                submenu: None,
            },
            SettingItem {
                id: "hide-thinking".to_string(),
                label: "Hide thinking".to_string(),
                description: Some("Hide thinking blocks in assistant responses".to_string()),
                current_value: if options.hide_thinking_block { "true" } else { "false" }.to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "mermaid-rendering".to_string(),
                label: "Mermaid diagrams".to_string(),
                description: Some(
                    "Render Mermaid code blocks as Unicode diagrams".to_string(),
                ),
                current_value: options.mermaid_rendering_mode.as_str().to_string(),
                values: Some(vec![
                    "off".to_string(),
                    "final".to_string(),
                    "streaming".to_string(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "cache-miss-notices".to_string(),
                label: "Cache miss notices".to_string(),
                description: Some(
                    "Show transcript notices for cache costs and provider recovery diagnostics"
                        .to_string(),
                ),
                current_value: if options.show_cache_miss_notices { "true" } else { "false" }.to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "collapse-changelog".to_string(),
                label: "Collapse changelog".to_string(),
                description: Some("Show condensed changelog after updates".to_string()),
                current_value: if options.collapse_changelog { "true" } else { "false" }.to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "quiet-startup".to_string(),
                label: "Quiet startup".to_string(),
                description: Some("Disable verbose printing at startup".to_string()),
                current_value: if options.quiet_startup { "true" } else { "false" }.to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "install-telemetry".to_string(),
                label: "Install telemetry".to_string(),
                description: Some(
                    "Send an anonymous version/update ping after changelog-detected updates"
                        .to_string(),
                ),
                current_value: if options.enable_install_telemetry { "true" } else { "false" }.to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "default-project-trust".to_string(),
                label: "Default project trust".to_string(),
                description: Some(
                    "Fallback behavior when no extension or saved trust decision decides project trust".to_string(),
                ),
                current_value: trust_label(options.default_project_trust).to_string(),
                values: Some(
                    DEFAULT_PROJECT_TRUST_LABELS
                        .iter()
                        .map(|(_, label)| label.to_string())
                        .collect(),
                ),
                submenu: None,
            },
            SettingItem {
                id: "double-escape-action".to_string(),
                label: "Double-escape action".to_string(),
                description: Some("Action when pressing Escape twice with empty editor".to_string()),
                current_value: double_escape_to_str(options.double_escape_action).to_string(),
                values: Some(vec![
                    "tree".to_string(),
                    "fork".to_string(),
                    "none".to_string(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "tree-filter-mode".to_string(),
                label: "Tree filter mode".to_string(),
                description: Some("Default filter when opening /tree".to_string()),
                current_value: tree_filter_to_str(options.tree_filter_mode).to_string(),
                values: Some(vec![
                    "default".to_string(),
                    "no-tools".to_string(),
                    "user-only".to_string(),
                    "labeled-only".to_string(),
                    "all".to_string(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "warnings".to_string(),
                label: "Warnings".to_string(),
                description: Some("Enable or disable individual warnings".to_string()),
                current_value: "configure".to_string(),
                values: None,
                submenu: Some({
                    let on_change = Arc::clone(&on_change);
                    let settings_list_theme = Arc::clone(&settings_list_theme);
                    let current_warnings = Arc::clone(&current_warnings);
                    Box::new(move |_current_value: &str, done: SubmenuDone| {
                        let done = Arc::new(Mutex::new(Some(done)));
                        let warnings = lock(&current_warnings).clone();
                        // Clone into fresh locals so the inner `move`
                        // closures capture those instead of moving the
                        // outer captures out of this FnMut.
                        let current_warnings = Arc::clone(&current_warnings);
                        let on_change = Arc::clone(&on_change);
                        let on_warnings_change = Box::new(move |new_warnings: WarningSettings| {
                            *lock(&current_warnings) = new_warnings.clone();
                            lock(&on_change)(SettingsChange::Warnings(new_warnings));
                        });
                        let on_cancel: Box<dyn FnMut() + Send> =
                            Box::new(move || call_done(&done, None));
                        Box::new(WarningSettingsSubmenu::new(
                            warnings,
                            Arc::clone(&settings_list_theme),
                            on_warnings_change,
                            on_cancel,
                        ))
                    })
                }),
            },
            // "model-thinking" (settings-selector.ts:577-662 @ 2ff8ba622 +
            // ee29aa118 searchable + a669db3c3 labels + f2a622789 markers):
            // per-model thinking overrides. The plain "Default thinking
            // level" entry was REMOVED upstream (5b3caaf4c — Ctrl+S in
            // /thinking is the default-setting path).
            SettingItem {
                id: "model-thinking".to_string(),
                label: "Default thinking level per model".to_string(),
                description: Some(format!(
                    "Override the default thinking level for specific models. {} cycles in-session.",
                    crate::modes::interactive::components::keybinding_hints::key_text(
                        "app.thinking.cycle"
                    )
                )),
                current_value: model_thinking_overrides_summary(&options.model_thinking_levels)
                    .to_string(),
                values: None,
                submenu: Some({
                    let on_change = Arc::clone(&on_change);
                    let theme = Arc::clone(&theme);
                    let select_list_theme = Arc::clone(&select_list_theme);
                    let available_models = options.available_default_models.clone();
                    let current_model = options.current_model.clone();
                    let default_model = options.default_model.clone();
                    let thinking_level = options.thinking_level;
                    let overrides = Arc::clone(&current_model_thinking_levels);
                    Box::new(move |_current_value: &str, done: SubmenuDone| {
                        Box::new(ModelThinkingSubmenu::new(
                            available_models.clone(),
                            current_model.clone(),
                            default_model.clone(),
                            Arc::clone(&overrides),
                            thinking_level,
                            Arc::clone(&theme),
                            Arc::clone(&select_list_theme),
                            {
                                let on_change = Arc::clone(&on_change);
                                let overrides = Arc::clone(&overrides);
                                Box::new(
                                    move |provider: &str,
                                          model_id: &str,
                                          level: rpi_agent::types::ThinkingLevel| {
                                        lock(&overrides).insert(
                                            format!("{provider}/{model_id}"),
                                            level,
                                        );
                                        lock(&on_change)(SettingsChange::ModelThinkingLevelChange {
                                            provider: provider.to_string(),
                                            model_id: model_id.to_string(),
                                            level,
                                        });
                                    },
                                )
                            },
                            {
                                let on_change = Arc::clone(&on_change);
                                let overrides = Arc::clone(&overrides);
                                Box::new(move |provider: &str, model_id: &str| {
                                    lock(&overrides).remove(&format!("{provider}/{model_id}"));
                                    lock(&on_change)(SettingsChange::ModelThinkingLevelRemove {
                                        provider: provider.to_string(),
                                        model_id: model_id.to_string(),
                                    });
                                })
                            },
                            {
                                // Cancel/done writes the fresh summary back
                                // to the item's current value
                                // (SteppedSubmenu `done(summary())`).
                                let done = Arc::new(Mutex::new(Some(done)));
                                let overrides = Arc::clone(&overrides);
                                Box::new(move || {
                                    let summary = model_thinking_overrides_summary(&lock(&overrides))
                                        .to_string();
                                    call_done(&done, Some(summary));
                                })
                            },
                        )) as Box<dyn rpi_tui::tui::Component + Send>
                    })
                }),
            },
            // T32: tui-mode, fullscreen-exit-output, fullscreen-scrollbar
            // (settings-selector.ts:636-657 @ 5446cd754/6129a353b).
            SettingItem {
                id: "tui-mode".to_string(),
                label: "TUI mode".to_string(),
                description: Some(
                    "Interface layout; fullscreen mode is experimental".to_string(),
                ),
                current_value: tui_mode_to_str(options.tui_mode).to_string(),
                values: Some(vec!["regular".to_string(), "fullscreen".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "fullscreen-exit-output".to_string(),
                label: "Fullscreen exit output".to_string(),
                description: Some(
                    "Print the transcript or only a session resume hint when exiting fullscreen mode"
                        .to_string(),
                ),
                current_value: fullscreen_exit_output_to_str(options.fullscreen_exit_output)
                    .to_string(),
                values: Some(vec![
                    "transcript".to_string(),
                    "resume-hint".to_string(),
                ]),
                submenu: None,
            },
            SettingItem {
                id: "fullscreen-scrollbar".to_string(),
                label: "Fullscreen scrollbar".to_string(),
                description: Some(
                    "Scrollbar behavior in fullscreen mode; has no effect in regular mode"
                        .to_string(),
                ),
                current_value: fullscreen_scrollbar_to_str(options.fullscreen_scrollbar)
                    .to_string(),
                values: Some(vec![
                    "auto".to_string(),
                    "always".to_string(),
                    "hidden".to_string(),
                ]),
                submenu: None,
            },
            // settings-selector.ts:698-704 @ 9841914, 4e4949299.
            SettingItem {
                id: "fullscreen-copy-on-select".to_string(),
                label: "Fullscreen copy on select".to_string(),
                description: Some(
                    "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X"
                        .to_string(),
                ),
                current_value: if options.fullscreen_copy_on_select {
                    "true".to_string()
                } else {
                    "false".to_string()
                },
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
            SettingItem {
                id: "theme".to_string(),
                label: "Theme".to_string(),
                description: Some("Color theme for the interface".to_string()),
                current_value: options.current_theme.clone(),
                values: None,
                submenu: Some({
                    let theme = Arc::clone(&theme);
                    let settings_list_theme = Arc::clone(&settings_list_theme);
                    let select_list_theme = Arc::clone(&select_list_theme);
                    let terminal_theme = options.terminal_theme;
                    let available_themes = Arc::new(options.available_themes.clone());
                    let on_change = Arc::clone(&on_change);
                    Box::new(move |current_value: &str, done: SubmenuDone| {
                        let on_theme_preview: Box<dyn FnMut(&str) + Send> = Box::new({
                            let on_change = Arc::clone(&on_change);
                            move |name: &str| {
                                lock(&on_change)(SettingsChange::ThemePreview(name.to_string()))
                            }
                        });
                        Box::new(ThemeSubmenu::new(
                            current_value,
                            terminal_theme,
                            Arc::clone(&available_themes),
                            Arc::clone(&theme),
                            Arc::clone(&settings_list_theme),
                            Arc::clone(&select_list_theme),
                            Some(on_theme_preview),
                            done,
                        ))
                    })
                }),
            },
        ];

        // Only show image toggle if terminal supports it
        // (settings-selector.ts:624-640).
        if supports_images {
            // Insert after autocompact.
            items.insert(
                1,
                SettingItem {
                    id: "show-images".to_string(),
                    label: "Show images".to_string(),
                    description: Some("Render images inline in terminal".to_string()),
                    current_value: if options.show_images { "true" } else { "false" }.to_string(),
                    values: Some(vec!["true".to_string(), "false".to_string()]),
                    submenu: None,
                },
            );
            items.insert(
                2,
                SettingItem {
                    id: "image-width-cells".to_string(),
                    label: "Image width".to_string(),
                    description: Some("Preferred inline image width in terminal cells".to_string()),
                    current_value: options.image_width_cells.to_string(),
                    values: Some(vec!["60".to_string(), "80".to_string(), "120".to_string()]),
                    submenu: None,
                },
            );
        }

        // Image auto-resize toggle (always available, affects both attached
        // and read images) (settings-selector.ts:642-649).
        items.insert(
            if supports_images { 3 } else { 1 },
            SettingItem {
                id: "auto-resize-images".to_string(),
                label: "Auto-resize images".to_string(),
                description: Some(
                    "Resize large images to 2000x2000 max for better model compatibility"
                        .to_string(),
                ),
                current_value: if options.auto_resize_images {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
        );

        // Block images toggle (always available, insert after
        // auto-resize-images) (settings-selector.ts:651-659).
        insert_after(
            &mut items,
            "auto-resize-images",
            SettingItem {
                id: "block-images".to_string(),
                label: "Block images".to_string(),
                description: Some("Prevent images from being sent to LLM providers".to_string()),
                current_value: if options.block_images {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
        );

        // Skill commands toggle (insert after block-images)
        // (settings-selector.ts:661-669).
        insert_after(
            &mut items,
            "block-images",
            SettingItem {
                id: "skill-commands".to_string(),
                label: "Skill commands".to_string(),
                description: Some("Register skills as /skill:name commands".to_string()),
                current_value: if options.enable_skill_commands {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
        );

        // Hardware cursor toggle (insert after skill-commands)
        // (settings-selector.ts:671-679).
        insert_after(
            &mut items,
            "skill-commands",
            SettingItem {
                id: "show-hardware-cursor".to_string(),
                label: "Show hardware cursor".to_string(),
                description: Some(
                    "Show the terminal cursor while still positioning it for IME support"
                        .to_string(),
                ),
                current_value: if options.show_hardware_cursor {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
        );

        // Editor padding toggle (insert after show-hardware-cursor)
        // (settings-selector.ts:681-689).
        insert_after(
            &mut items,
            "show-hardware-cursor",
            SettingItem {
                id: "editor-padding".to_string(),
                label: "Editor padding".to_string(),
                description: Some("Horizontal padding for input editor (0-3)".to_string()),
                current_value: options.editor_padding_x.to_string(),
                values: Some(vec![
                    "0".to_string(),
                    "1".to_string(),
                    "2".to_string(),
                    "3".to_string(),
                ]),
                submenu: None,
            },
        );

        // Output padding toggle (insert after editor-padding)
        // (settings-selector.ts:691-699).
        insert_after(
            &mut items,
            "editor-padding",
            SettingItem {
                id: "output-padding".to_string(),
                label: "Output padding".to_string(),
                description: Some(
                    "Horizontal padding for user messages, assistant messages, and thinking"
                        .to_string(),
                ),
                current_value: options.output_pad.to_string(),
                values: Some(vec!["0".to_string(), "1".to_string()]),
                submenu: None,
            },
        );

        // Autocomplete max visible toggle (insert after output-padding)
        // (settings-selector.ts:701-709).
        insert_after(
            &mut items,
            "output-padding",
            SettingItem {
                id: "autocomplete-max-visible".to_string(),
                label: "Autocomplete max items".to_string(),
                description: Some("Max visible items in autocomplete dropdown (3-20)".to_string()),
                current_value: options.autocomplete_max_visible.to_string(),
                values: Some(vec![
                    "3".to_string(),
                    "5".to_string(),
                    "7".to_string(),
                    "10".to_string(),
                    "15".to_string(),
                    "20".to_string(),
                ]),
                submenu: None,
            },
        );

        // Clear on shrink toggle (insert after autocomplete-max-visible)
        // (settings-selector.ts:711-719).
        insert_after(
            &mut items,
            "autocomplete-max-visible",
            SettingItem {
                id: "clear-on-shrink".to_string(),
                label: "Clear on shrink".to_string(),
                description: Some(
                    "Clear empty rows when content shrinks (may cause flicker)".to_string(),
                ),
                current_value: if options.clear_on_shrink {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
        );

        // Terminal progress toggle (insert after clear-on-shrink)
        // (settings-selector.ts:721-729).
        insert_after(
            &mut items,
            "clear-on-shrink",
            SettingItem {
                id: "terminal-progress".to_string(),
                label: "Terminal progress".to_string(),
                description: Some(
                    "Show OSC 9;4 progress indicators in the terminal tab bar".to_string(),
                ),
                current_value: if options.show_terminal_progress {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
                values: Some(vec!["true".to_string(), "false".to_string()]),
                submenu: None,
            },
        );

        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("border", text))
        };

        // `onChange` mapping (settings-selector.ts:738-826).
        let mut settings_list = SettingsList::new(
            items,
            10,
            settings_list_theme,
            Some(SettingsListOptions {
                enable_search: true,
            }),
        );
        settings_list.on_change = Some(Box::new(move |id, new_value| {
            let change = match id {
                "autocompact" => SettingsChange::AutoCompact(new_value == "true"),
                "show-images" => SettingsChange::ShowImages(new_value == "true"),
                "image-width-cells" => {
                    SettingsChange::ImageWidthCells(new_value.parse().unwrap_or(0))
                }
                "auto-resize-images" => SettingsChange::AutoResizeImages(new_value == "true"),
                "block-images" => SettingsChange::BlockImages(new_value == "true"),
                "skill-commands" => SettingsChange::EnableSkillCommands(new_value == "true"),
                "steering-mode" => SettingsChange::SteeringMode(parse_queue_mode(new_value)),
                "follow-up-mode" => SettingsChange::FollowUpMode(parse_queue_mode(new_value)),
                "transport" => SettingsChange::Transport(parse_transport(new_value)),
                "http-idle-timeout" => {
                    match HTTP_IDLE_TIMEOUT_CHOICES
                        .iter()
                        .find(|(label, _)| *label == new_value)
                    {
                        Some((_, timeout_ms)) => SettingsChange::HttpIdleTimeoutMs(*timeout_ms),
                        None => return,
                    }
                }
                "hide-thinking" => SettingsChange::HideThinkingBlock(new_value == "true"),
                "mermaid-rendering" => {
                    SettingsChange::MermaidRenderingMode(parse_mermaid_rendering_mode(new_value))
                }
                "cache-miss-notices" => SettingsChange::ShowCacheMissNotices(new_value == "true"),
                "collapse-changelog" => SettingsChange::CollapseChangelog(new_value == "true"),
                "quiet-startup" => SettingsChange::QuietStartup(new_value == "true"),
                "install-telemetry" => SettingsChange::EnableInstallTelemetry(new_value == "true"),
                "default-project-trust" => match trust_from_label(new_value) {
                    Some(trust) => SettingsChange::DefaultProjectTrust(trust),
                    None => return,
                },
                "double-escape-action" => {
                    SettingsChange::DoubleEscapeAction(parse_double_escape(new_value))
                }
                "tree-filter-mode" => SettingsChange::TreeFilterMode(parse_tree_filter(new_value)),
                "show-hardware-cursor" => SettingsChange::ShowHardwareCursor(new_value == "true"),
                "editor-padding" => SettingsChange::EditorPaddingX(new_value.parse().unwrap_or(0)),
                "output-padding" => SettingsChange::OutputPad(if new_value == "0" { 0 } else { 1 }),
                "autocomplete-max-visible" => {
                    SettingsChange::AutocompleteMaxVisible(new_value.parse().unwrap_or(0))
                }
                "clear-on-shrink" => SettingsChange::ClearOnShrink(new_value == "true"),
                "terminal-progress" => SettingsChange::ShowTerminalProgress(new_value == "true"),
                "tui-mode" => SettingsChange::TuiMode(parse_tui_mode(new_value)),
                "fullscreen-exit-output" => {
                    SettingsChange::FullscreenExitOutput(parse_fullscreen_exit_output(new_value))
                }
                "fullscreen-scrollbar" => {
                    SettingsChange::FullscreenScrollbar(parse_fullscreen_scrollbar(new_value))
                }
                "fullscreen-copy-on-select" => {
                    SettingsChange::FullscreenCopyOnSelect(new_value == "true")
                }
                "theme" => SettingsChange::Theme(new_value.to_string()),
                _ => return,
            };
            lock(&on_change)(change);
        }));
        settings_list.on_cancel = Some(on_cancel);

        Self {
            top_border: DynamicBorder::new(border_color.clone()),
            settings_list,
            bottom_border: DynamicBorder::new(border_color),
            focused: false,
        }
    }

    /// `getSettingsList` (settings-selector.ts:835-837).
    pub fn get_settings_list(&mut self) -> &mut SettingsList {
        &mut self.settings_list
    }
}

/// Upstream `items.splice(index, 0, item)` after an item id
/// (settings-selector.ts:651-729).
fn insert_after(items: &mut Vec<SettingItem>, after_id: &str, item: SettingItem) {
    let index = items
        .iter()
        .position(|i| i.id == after_id)
        .map_or(0, |i| i + 1);
    items.insert(index.min(items.len()), item);
}

impl Component for SettingsSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.top_border.render(width));
        lines.extend(self.settings_list.render(width));
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.settings_list.handle_input(data);
    }

    fn invalidate(&mut self) {
        self.settings_list.invalidate();
    }
}

impl Focusable for SettingsSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        // Upstream focuses `selector.getSettingsList()`, which forwards to
        // its search input; the local `SettingsList` is not `Focusable`, so
        // the flag is kept locally (the search input's cursor marker is not
        // emitted — cosmetic difference).
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::utils::visible_width;

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
    }

    fn install_keybindings() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
    }

    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn options() -> SettingsSelectorOptions {
        SettingsSelectorOptions {
            auto_compact: false,
            show_images: true,
            image_width_cells: 80,
            auto_resize_images: true,
            block_images: false,
            enable_skill_commands: true,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::All,
            transport: Transport::Auto,
            http_idle_timeout_ms: 300_000,
            thinking_level: ThinkingLevel::High,
            model_thinking_levels: Default::default(),
            available_default_models: Vec::new(),
            current_model: None,
            default_model: "not set".to_string(),
            current_theme: "dark".to_string(),
            terminal_theme: TerminalColorScheme::Dark,
            available_themes: vec!["dark".to_string(), "light".to_string()],
            hide_thinking_block: false,
            mermaid_rendering_mode: MermaidRenderingMode::Streaming,
            show_cache_miss_notices: true,
            collapse_changelog: false,
            enable_install_telemetry: true,
            double_escape_action: DoubleEscapeAction::Tree,
            tree_filter_mode: TreeFilterMode::Default,
            show_hardware_cursor: false,
            editor_padding_x: 1,
            output_pad: 0,
            autocomplete_max_visible: 7,
            quiet_startup: false,
            default_project_trust: DefaultProjectTrust::Ask,
            clear_on_shrink: true,
            show_terminal_progress: false,
            tui_mode: rpi_tui::tui::TuiMode::Regular,
            fullscreen_exit_output: crate::core::settings_manager::FullscreenExitOutput::Transcript,
            fullscreen_scrollbar: rpi_tui::components::scroll_view::ScrollbarMode::Auto,
            fullscreen_copy_on_select: true,
            warnings: WarningSettings {
                anthropic_extra_usage: Some(true),
            },
        }
    }

    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn changes() -> (
        Arc<Mutex<Vec<SettingsChange>>>,
        Box<dyn FnMut(SettingsChange) + Send>,
    ) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&received);
        let on_change: Box<dyn FnMut(SettingsChange) + Send> =
            Box::new(move |change| captured.lock().unwrap().push(change));
        (received, on_change)
    }

    /// Strip ANSI escape sequences.
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

    fn render_plain(component: &SettingsSelectorComponent, width: usize) -> Vec<String> {
        let lines = component.render(width);
        for line in &lines {
            assert!(
                visible_width(line) <= width,
                "line wider than {width}: {:?}",
                line
            );
        }
        plain(lines)
    }

    #[test]
    fn renders_main_panel() {
        install_keybindings();
        let (_received, on_change) = changes();
        let cancelled = Arc::new(Mutex::new(0usize));
        let on_cancel: Box<dyn FnMut() + Send> = Box::new({
            let cancelled = Arc::clone(&cancelled);
            move || {
                *cancelled.lock().unwrap() += 1;
            }
        });
        let component = SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        // Top border + first visible items + hint.
        assert!(joined.starts_with('─'));
        // The 10-row window always starts with Auto-compact and the rows
        // inserted right after it (image rows depend on terminal support).
        for label in [
            "Auto-compact",
            "Auto-resize images",
            "Block images",
            "Skill commands",
            "Show hardware cursor",
            "Editor padding",
            "Output padding",
            "Autocomplete max items",
        ] {
            assert!(joined.contains(label), "missing {label}");
        }
        // Values render (false for the boolean rows, a number for padding).
        assert!(joined.contains("false"));
        assert!(joined.contains("Autocomplete max items"));
        assert!(joined.contains("Enter/Space to change · Esc to cancel"));
        // Scroll hint shows the item count (30 items without image rows, 32
        // with them; +fullscreen-copy-on-select, 4e4949299; the 10-row
        // window always scrolls).
        let supports_images = get_capabilities().images.is_some();
        let item_count = if supports_images { 32 } else { 30 };
        assert!(lines
            .iter()
            .any(|l| l.contains(&format!("(1/{item_count})"))));
    }

    #[test]
    fn cycles_autocompact_and_fires_on_change() {
        install_keybindings();
        let (received, on_change) = changes();
        let cancelled = Arc::new(Mutex::new(0usize));
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(move || {
            *cancelled.lock().unwrap() += 1;
        });
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);

        // First item is Auto-compact (values false → true).
        component.handle_input("\r");
        let lines = render_plain(&component, 100);
        assert!(lines
            .iter()
            .any(|l| l.contains("Auto-compact") && l.contains("true")));
        assert_eq!(
            *received.lock().unwrap(),
            vec![SettingsChange::AutoCompact(true)]
        );

        // Space cycles again (true → false).
        component.handle_input(" ");
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                SettingsChange::AutoCompact(true),
                SettingsChange::AutoCompact(false)
            ]
        );
    }

    #[test]
    fn cycles_transport_and_maps_values() {
        install_keybindings();
        let (received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Jump to Transport through the search box (position-independent of
        // the image-row layout).
        for c in ["t", "r", "a", "n", "s"] {
            component.handle_input(c);
        }
        component.handle_input("\r");
        let events = received.lock().unwrap();
        // auto → sse (first value after current).
        assert_eq!(events[0], SettingsChange::Transport(Transport::Sse));
    }

    #[test]
    fn cycles_mermaid_rendering_and_maps_values() {
        install_keybindings();
        let (received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Jump to Mermaid diagrams through the search box.
        for c in ["m", "e", "r"] {
            component.handle_input(c);
        }
        component.handle_input("\r");
        let events = received.lock().unwrap();
        // streaming → off (first value after current).
        assert_eq!(
            events[0],
            SettingsChange::MermaidRenderingMode(MermaidRenderingMode::Off)
        );
        // The current value renders next to the label.
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Mermaid diagrams") && joined.contains("off"));
    }

    #[test]
    fn escape_cancels() {
        install_keybindings();
        let (_received, on_change) = changes();
        let cancelled = Arc::new(Mutex::new(0usize));
        let on_cancel: Box<dyn FnMut() + Send> = Box::new({
            let cancelled = Arc::clone(&cancelled);
            move || {
                *cancelled.lock().unwrap() += 1;
            }
        });
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn search_filters_settings() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Search input is enabled; type "trans".
        component.handle_input("t");
        component.handle_input("r");
        component.handle_input("a");
        component.handle_input("n");
        component.handle_input("s");
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Transport"));
        assert!(!joined.contains("Steering mode"));
    }

    #[test]
    fn update_value_reflects_in_rendering() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        component
            .get_settings_list()
            .update_value("autocompact", "true");
        let lines = render_plain(&component, 100);
        assert!(lines
            .iter()
            .any(|l| l.contains("Auto-compact") && l.contains("true")));
    }

    #[test]
    fn warnings_submenu_updates_state_and_fires_warnings_change() {
        install_keybindings();
        let (received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Move down to the Warnings item (index 25 with image rows, 23
        // without: the nine always-present rows inserted after autocompact /
        // auto-resize push the base items down).
        let target = if get_capabilities().images.is_some() {
            25
        } else {
            23
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        // Submenu renders its single item.
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Anthropic extra usage"));
        // Cycle the value: true → false.
        component.handle_input("\r");
        let events = received.lock().unwrap();
        assert!(events.iter().any(
            |e| matches!(e, SettingsChange::Warnings(w) if w.anthropic_extra_usage == Some(false))
        ));
        // Esc closes the submenu back to the main panel.
        component.handle_input("\x1b");
        let lines = render_plain(&component, 100);
        // The main panel is back (selection restored to the Warnings row,
        // which sits in the scrolling window; the submenu content is gone).
        assert!(lines.join("\n").contains("→ Warnings"));
        assert!(!lines.join("\n").contains("Anthropic extra usage"));
    }

    /// "model-thinking" per-model overrides submenu (2ff8ba622 +
    /// ee29aa118 + a669db3c3 + f2a622789). The old "thinking" entry test
    /// (session-level select via settings) is retired with the entry
    /// (5b3caaf4c, G2 registered).
    #[test]
    fn model_thinking_submenu_sets_and_clears_overrides() {
        install_keybindings();
        let (received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut opts = options();
        // Two reasoning models; the current one sorts first.
        opts.available_default_models = vec![
            test_model("alpha", "a1", true),
            test_model("beta", "b1", true),
        ];
        opts.current_model = Some(test_model("beta", "b1", true));
        let mut component = SettingsSelectorComponent::new(opts, theme(), on_change, on_cancel);
        // Move to the model-thinking item (same index the old thinking
        // entry occupied: 26 with image rows, 24 without).
        let target = if get_capabilities().images.is_some() {
            26
        } else {
            24
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        let joined = render_plain(&component, 100).join("\n");
        // Step 1: searchable model picker, current model first,
        // `modelid [provider]` labels (a669db3c3).
        assert!(joined.contains("Per-Model Thinking Level"));
        assert!(joined.contains("Step 1/2"));
        assert!(joined.contains("Type to filter"), "searchable hint");
        let b1 = joined
            .lines()
            .find(|l| l.contains("b1 [beta]"))
            .expect("current model listed");
        let b1_index = joined.lines().position(|l| l == b1).unwrap();
        let a1_index = joined
            .lines()
            .position(|l| l.contains("a1 [alpha]"))
            .expect("alpha listed");
        assert!(b1_index < a1_index, "current model pinned first");

        // Enter on b1 → step 2: level picker with ✓ on nothing yet and no
        // clear-override row.
        component.handle_input("\r");
        let joined = render_plain(&component, 100).join("\n");
        assert!(joined.contains("Step 2/2"));
        assert!(joined.contains("Thinking Level for b1 [beta]"));
        assert!(!joined.contains("(clear override)"));

        // Select high → change event + loop back to step 1 with the
        // override shown as the description.
        component.handle_input("\r"); // first level (off — EXTENDED list head)
        let events = received.lock().unwrap();
        assert!(events.contains(&SettingsChange::ModelThinkingLevelChange {
            provider: "beta".to_string(),
            model_id: "b1".to_string(),
            level: rpi_agent::types::ThinkingLevel::Off,
        }));
        drop(events);
        let joined = render_plain(&component, 100).join("\n");
        assert!(joined.contains("Step 1/2"));
        assert!(joined.contains("off"), "override as description");

        // Re-enter b1 → ✓ off + (clear override) row.
        component.handle_input("\r");
        let joined = render_plain(&component, 100).join("\n");
        assert!(joined.contains("\u{2713} off"), "active override marked");
        assert!(joined.contains("(clear override)"));
        // Move down to the clear row (last of 6: off/minimal/low/medium/
        // high + clear; the list wraps, so 5 downs land on it).
        for _ in 0..5 {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        let events = received.lock().unwrap();
        assert!(events.contains(&SettingsChange::ModelThinkingLevelRemove {
            provider: "beta".to_string(),
            model_id: "b1".to_string(),
        }));
    }

    #[test]
    fn model_thinking_submenu_esc_goes_back_and_cancels() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut opts = options();
        opts.available_default_models = vec![test_model("alpha", "a1", true)];
        let mut component = SettingsSelectorComponent::new(opts, theme(), on_change, on_cancel);
        let target = if get_capabilities().images.is_some() {
            26
        } else {
            24
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r"); // open submenu
        component.handle_input("\r"); // → level step
                                      // Esc at step 2 goes back to step 1 (SteppedSubmenu).
        component.handle_input("\x1b");
        let joined = render_plain(&component, 100).join("\n");
        assert!(joined.contains("Step 1/2"), "esc backs to model step");
        // Esc at step 1 closes the submenu and writes the summary back.
        component.handle_input("\x1b");
        let joined = render_plain(&component, 100).join("\n");
        assert!(joined.contains("Default thinking level per model"));
        assert!(joined.contains("not set"));
    }

    /// Non-reasoning models offer only "off" (settings-selector.ts:629-631).
    #[test]
    fn model_thinking_submenu_non_reasoning_model_offers_off() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut opts = options();
        opts.available_default_models = vec![test_model("alpha", "a1", false)];
        let mut component = SettingsSelectorComponent::new(opts, theme(), on_change, on_cancel);
        let target = if get_capabilities().images.is_some() {
            26
        } else {
            24
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r"); // open submenu
        component.handle_input("\r"); // → level step
        let joined = render_plain(&component, 100).join("\n");
        assert!(joined.contains("off"));
        assert!(joined.contains("No reasoning"));
    }

    fn test_model(provider: &str, id: &str, reasoning: bool) -> rpi_ai::types::Model {
        use rpi_ai::types::{ApiKind, InputModality, ModelCost, ModelCostRates};
        rpi_ai::types::Model {
            name: format!("{provider} {id}"),
            id: id.to_string(),
            api: ApiKind("anthropic-messages".to_string()),
            provider: provider.to_string(),
            base_url: "https://example.invalid".to_string(),
            reasoning,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost {
                rates: ModelCostRates::default(),
                tiers: None,
            },
            context_window: 200_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            sampling_params: None,
        }
    }

    #[test]
    fn theme_submenu_switches_single_and_automatic_modes() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Theme item index: 30 with image rows, 28 without (3 T32 items
        // added before theme: tui-mode, fullscreen-exit-output, fullscreen-scrollbar).
        let target = if get_capabilities().images.is_some() {
            31
        } else {
            29
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Theme"));
        assert!(joined.contains("Automatic"));
        assert!(joined.contains("Enter to select · Esc to go back"));
        // The current theme ("dark") is pre-selected; move up to
        // "Automatic" → automatic mode menu.
        component.handle_input("\x1b[A");
        component.handle_input("\r");
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Automatic Theme"));
        assert!(joined.contains("Light theme"));
        assert!(joined.contains("Dark theme"));
        assert!(joined.contains("Apply"));
        assert!(joined.contains("Change mode"));
        // "Change mode" (4th item) switches back to the single menu.
        for _ in 0..3 {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        let lines = render_plain(&component, 100);
        assert!(lines
            .join("\n")
            .contains("Select a theme, or choose Automatic"));
        // Esc cancels the theme submenu back to the main panel.
        component.handle_input("\x1b");
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("→ Theme"));
        assert!(!joined.contains("Enter to select · Esc to go back"));
    }

    #[test]
    fn automatic_theme_apply_writes_combined_setting() {
        install_keybindings();
        let (received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Theme item index: 30 with image rows, 28 without (3 T32 items
        // added before theme: tui-mode, fullscreen-exit-output, fullscreen-scrollbar).
        let target = if get_capabilities().images.is_some() {
            31
        } else {
            29
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        // Choose Automatic (above the pre-selected "dark").
        component.handle_input("\x1b[A");
        component.handle_input("\r");
        // Choose a light theme via the Light theme submenu (first item).
        component.handle_input("\r");
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Light Theme"));
        // Select the "light" theme (second item) and confirm.
        component.handle_input("\x1b[B");
        component.handle_input("\r");
        let events = received.lock().unwrap();
        // Preview events fired during selection, then the write-back.
        assert!(events
            .iter()
            .any(|e| matches!(e, SettingsChange::ThemePreview(name) if name == "light/dark")));
        // Back on the automatic menu; pick "Apply" (3rd item) → Theme event
        // with the combined setting.
        drop(events);
        for _ in 0..2 {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        let events = received.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| *e == SettingsChange::Theme("light/dark".to_string())));
    }

    #[test]
    fn t32_items_appear_with_correct_count() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let component = SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);

        // The full item count is now 30 (no images) or 32 (with images):
        // 26 base + 3 T32 items + fullscreen-copy-on-select (4e4949299).
        let supports_images = get_capabilities().images.is_some();
        let expected = if supports_images { 32 } else { 30 };
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        // The scroll indicator shows the total count.
        assert!(
            joined.contains(&format!("{expected}")),
            "total item count {expected} visible in scroll indicator"
        );
    }
}
