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
    DefaultProjectTrust, DoubleEscapeAction, TransportSetting, TreeFilterMode, WarningSettings,
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

/// `THINKING_DESCRIPTIONS` (settings-selector.ts:32-40), minus the "off"
/// entry (see module header).
const THINKING_DESCRIPTIONS: [(&str, &str); 6] = [
    ("minimal", "Very brief reasoning (~1k tokens)"),
    ("low", "Light reasoning (~2k tokens)"),
    ("medium", "Moderate reasoning (~8k tokens)"),
    ("high", "Deep reasoning (~16k tokens)"),
    ("xhigh", "Extra-high reasoning (~32k tokens)"),
    ("max", "Maximum reasoning"),
];

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
    pub thinking_level: ThinkingLevel,
    pub available_thinking_levels: Vec<ThinkingLevel>,
    pub current_theme: String,
    /// Upstream `TerminalTheme` (`"dark" | "light"`).
    pub terminal_theme: TerminalColorScheme,
    pub available_themes: Vec<String>,
    pub hide_thinking_block: bool,
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
    ThinkingLevel(ThinkingLevel),
    Theme(String),
    ThemePreview(String),
    HideThinkingBlock(bool),
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

fn parse_thinking_level(value: &str) -> ThinkingLevel {
    match value {
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        "max" => ThinkingLevel::Max,
        _ => ThinkingLevel::Minimal,
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

/// `SelectSubmenu` (settings-selector.ts:162-224): title + optional
/// description + `SelectList` + hint.
struct SelectSubmenu {
    title: String,
    description: String,
    select_list: SelectList,
    theme: Arc<Theme>,
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
    ) -> Self {
        let current_index = options.iter().position(|o| o.value == current_value);
        let max_visible = options.len().min(10);
        let mut select_list = SelectList::new(
            options,
            max_visible,
            select_list_theme,
            Some(SETTINGS_SUBMENU_SELECT_LIST_LAYOUT),
        );
        if let Some(current_index) = current_index {
            select_list.set_selected_index(current_index);
        }
        select_list.on_select = on_select;
        select_list.on_cancel = on_cancel;
        select_list.on_selection_change = on_selection_change;
        Self {
            title: title.to_string(),
            description: description.to_string(),
            select_list,
            theme,
        }
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
        lines.push(String::new());
        lines.extend(self.select_list.render(width));
        lines.push(String::new());
        lines.push(self.theme.fg("dim", "  Enter to select · Esc to go back"));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.select_list.handle_input(data);
    }
}

// ---------------------------------------------------------------------------
// Theme submenu (settings-selector.ts:226-467)
// ---------------------------------------------------------------------------

/// `themeItems` (settings-selector.ts:226-228).
fn theme_items(available_themes: &[String]) -> Vec<SelectItem> {
    available_themes
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: name.clone(),
            description: None,
        })
        .collect()
}

/// `singleModeThemeItems` (settings-selector.ts:232-241).
fn single_mode_theme_items(available_themes: &[String]) -> Vec<SelectItem> {
    let mut items = vec![SelectItem {
        value: AUTOMATIC_THEME_VALUE.to_string(),
        label: "Automatic".to_string(),
        description: Some("Use separate themes for light and dark terminal appearance".to_string()),
    }];
    items.extend(theme_items(available_themes));
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
        single_mode_theme_items(&inner.available_themes)
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

        let (theme, select_list_theme, available_themes) = {
            let inner = lock(&inner);
            (
                Arc::clone(&inner.theme),
                Arc::clone(&inner.select_list_theme),
                inner.available_themes.clone(),
            )
        };
        let select = SelectSubmenu::new(
            title,
            description,
            theme_items(&available_themes),
            current_value,
            theme,
            select_list_theme,
            Some(on_select),
            Some(on_cancel),
            Some(on_selection_change),
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
                id: "cache-miss-notices".to_string(),
                label: "Cache miss notices".to_string(),
                description: Some(
                    "Show transcript notices for significant prompt-cache misses".to_string(),
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
            SettingItem {
                id: "thinking".to_string(),
                label: "Thinking level".to_string(),
                description: Some("Reasoning depth for thinking-capable models".to_string()),
                current_value: thinking_level_to_str(options.thinking_level).to_string(),
                values: None,
                submenu: Some({
                    let on_change = Arc::clone(&on_change);
                    let theme = Arc::clone(&theme);
                    let select_list_theme = Arc::clone(&select_list_theme);
                    let levels = options.available_thinking_levels.clone();
                    Box::new(move |current_value: &str, done: SubmenuDone| {
                        let done = Arc::new(Mutex::new(Some(done)));
                        let items: Vec<SelectItem> = levels
                            .iter()
                            .map(|level| {
                                let name = thinking_level_to_str(*level);
                                SelectItem {
                                    value: name.to_string(),
                                    label: name.to_string(),
                                    description: THINKING_DESCRIPTIONS
                                        .iter()
                                        .find(|(n, _)| *n == name)
                                        .map(|(_, d)| d.to_string()),
                                }
                            })
                            .collect();
                        // Clone into fresh locals so the inner `move`
                        // closures capture those instead of moving the
                        // outer captures out of this FnMut.
                        let on_change = Arc::clone(&on_change);
                        let done_select = Arc::clone(&done);
                        let on_select: SelectItemFn = Box::new(move |item: &SelectItem| {
                            lock(&on_change)(SettingsChange::ThinkingLevel(
                                parse_thinking_level(&item.value),
                            ));
                            call_done(&done_select, Some(item.value.clone()));
                        });
                        let on_cancel: Box<dyn FnMut() + Send> =
                            Box::new(move || call_done(&done, None));
                        Box::new(SelectSubmenu::new(
                            "Thinking Level",
                            "Select reasoning depth for thinking-capable models",
                            items,
                            current_value,
                            Arc::clone(&theme),
                            Arc::clone(&select_list_theme),
                            Some(on_select),
                            Some(on_cancel),
                            None,
                        ))
                    })
                }),
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
            available_thinking_levels: vec![
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
                ThinkingLevel::Max,
            ],
            current_theme: "dark".to_string(),
            terminal_theme: TerminalColorScheme::Dark,
            available_themes: vec!["dark".to_string(), "light".to_string()],
            hide_thinking_block: false,
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
        // Scroll hint shows the item count (25 items without image rows, 27
        // with them; the 10-row window always scrolls).
        let supports_images = get_capabilities().images.is_some();
        let item_count = if supports_images { 27 } else { 25 };
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
        // Move down to the Warnings item (index 24 with image rows, 22
        // without: the nine always-present rows inserted after autocompact /
        // auto-resize push the base items down).
        let target = if get_capabilities().images.is_some() {
            24
        } else {
            22
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

    #[test]
    fn thinking_submenu_selects_level_and_writes_back() {
        install_keybindings();
        let (received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Move to the Thinking level item (index 25 with image rows, 23
        // without).
        let target = if get_capabilities().images.is_some() {
            25
        } else {
            23
        };
        for _ in 0..target {
            component.handle_input("\x1b[B");
        }
        component.handle_input("\r");
        let lines = render_plain(&component, 100);
        let joined = lines.join("\n");
        assert!(joined.contains("Thinking Level"));
        assert!(joined.contains("Deep reasoning (~16k tokens)"));
        // The current level (high) is pre-selected; move up to medium and
        // confirm.
        component.handle_input("\x1b[A");
        component.handle_input("\r");
        let events = received.lock().unwrap();
        assert!(events.contains(&SettingsChange::ThinkingLevel(ThinkingLevel::Medium)));
        // Done wrote the value back to the item.
        let lines = render_plain(&component, 100);
        assert!(lines
            .iter()
            .any(|l| l.contains("Thinking level") && l.contains("medium")));
    }

    #[test]
    fn theme_submenu_switches_single_and_automatic_modes() {
        install_keybindings();
        let (_received, on_change) = changes();
        let on_cancel: Box<dyn FnMut() + Send> = Box::new(|| {});
        let mut component =
            SettingsSelectorComponent::new(options(), theme(), on_change, on_cancel);
        // Theme item index: 26 with image rows, 24 without.
        let target = if get_capabilities().images.is_some() {
            26
        } else {
            24
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
        let target = if get_capabilities().images.is_some() {
            26
        } else {
            24
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
}
