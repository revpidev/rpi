//! Port of `first_time_setup.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - The theme options are injected as `themes: Vec<String>` (theme names)
//!   instead of the hardcoded `THEME_OPTIONS` (dark/light,
//!   first-time-setup.ts:19-22) — the same injection style as the
//!   theme-selector component (theme-selector.ts:27-32). `detected_theme` is
//!   `Option<String>`: `None` skips the "Detected system appearance" line
//!   (upstream always renders it, first-time-setup.ts:62).
//! - Both steps render their options through a themed
//!   [`pir_tui::components::select_list::SelectList`] (per the local spec)
//!   instead of upstream's manual `addOptionList` Text rows
//!   (first-time-setup.ts:103-110). The list drives selection marks and
//!   scrolling; note that unselected options render in the default terminal
//!   color — `SelectList` has no unselected-text theme callback, upstream
//!   wraps them in `theme.fg("text", ...)`.
//! - `onThemePreview` (first-time-setup.ts:117, 14) is an optional
//!   `on_theme_preview` hook; upstream requires it in the options.
//! - The analytics copy says "pir" instead of "Pi"
//!   (first-time-setup.ts:72-78); `APP_NAME` comes from
//!   `crate::config::APP_NAME`.
//! - No telemetry is sent from the component: upstream submits the result and
//!   the caller persists it; the port just reports
//!   [`FirstTimeSetupResult`] via `on_submit` and lets the integration layer
//!   apply it (`set_enable_install_telemetry` / `set_enable_analytics` exist
//!   but have no sending logic in pir).
//! - The literal `k` / `j` / `\n` key fallbacks are kept
//!   (first-time-setup.ts:127-131).

use std::sync::Arc;

use pir_tui::components::select_list::{SelectItem, SelectList, SelectListTheme};
use pir_tui::components::text::Text;
use pir_tui::tui::{Component, Focusable};

use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{key_hint, raw_key_hint};

/// `FirstTimeSetupResult` (first-time-setup.ts:7-10): theme name + analytics
/// opt-in. `analytics == true` means the user opted in.
pub struct FirstTimeSetupResult {
    pub theme: String,
    pub analytics: bool,
}

/// `SETUP_LOGO_LINES` (first-time-setup.ts:29).
const SETUP_LOGO_LINES: [&str; 4] = ["██████", "██  ██", "████  ██", "██    ██"];

/// `ANALYTICS_OPTIONS` (first-time-setup.ts:24-27).
const ANALYTICS_OPTIONS: [(&str, &str); 2] = [
    ("true", "Share anonymous usage data"),
    ("false", "Don't share"),
];

/// Setup wizard step (upstream `step: "theme" | "analytics"`,
/// first-time-setup.ts:33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Theme,
    Analytics,
}

/// `FirstTimeSetupComponent` (first-time-setup.ts:32-144): a two-step
/// first-run wizard — theme selection, then analytics opt-in. Both steps
/// render their options through a [`SelectList`]; Enter advances/submits,
/// Escape skips the setup, up/down (+ `k`/`j`) navigate.
pub struct FirstTimeSetupComponent {
    step: SetupStep,
    theme: Arc<Theme>,
    themes: Vec<String>,
    theme_index: usize,
    analytics_index: usize,
    detected_theme: Option<String>,
    theme_list: SelectList,
    analytics_list: SelectList,
    on_submit: Option<Box<dyn FnMut(FirstTimeSetupResult) + Send>>,
    on_cancel: Option<Box<dyn FnMut() + Send>>,
    /// Called when the theme selection moves (upstream `onThemePreview`,
    /// first-time-setup.ts:117).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_theme_preview: Option<Box<dyn FnMut(&str) + Send>>,
    /// TUI focus flag (the component's TUI entry delegates focus here).
    focused: bool,
}

/// A `SelectListTheme` styled with the current theme: accent selection,
/// muted descriptions/scroll info/no-match (upstream styles its option rows
/// with `theme.fg("accent"|"text")`, first-time-setup.ts:106-107).
fn select_list_theme(theme: &Arc<Theme>) -> SelectListTheme {
    let selected_text = Arc::clone(theme);
    let description = Arc::clone(theme);
    let scroll_info = Arc::clone(theme);
    let no_match = Arc::clone(theme);
    SelectListTheme {
        selected_prefix: Box::new(|text: &str| text.to_string()),
        selected_text: Box::new(move |text: &str| selected_text.fg("accent", text)),
        description: Box::new(move |text: &str| description.fg("muted", text)),
        scroll_info: Box::new(move |text: &str| scroll_info.fg("muted", text)),
        no_match: Box::new(move |text: &str| no_match.fg("muted", text)),
    }
}

impl FirstTimeSetupComponent {
    /// `constructor` (first-time-setup.ts:38-46): the theme step preselects
    /// `detected_theme` when present, else the first injected theme.
    pub fn new(
        theme: Arc<Theme>,
        themes: Vec<String>,
        detected_theme: Option<String>,
        on_submit: Box<dyn FnMut(FirstTimeSetupResult) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        let theme_index = detected_theme
            .as_deref()
            .and_then(|detected| themes.iter().position(|candidate| candidate == detected))
            .unwrap_or(0);

        let theme_items: Vec<SelectItem> = themes
            .iter()
            .map(|name| SelectItem {
                value: name.clone(),
                label: name.clone(),
                description: None,
            })
            .collect();
        let analytics_items: Vec<SelectItem> = ANALYTICS_OPTIONS
            .iter()
            .map(|(value, label)| SelectItem {
                value: (*value).to_string(),
                label: (*label).to_string(),
                description: None,
            })
            .collect();

        let list_theme = Arc::new(select_list_theme(&theme));
        let mut theme_list = SelectList::new(theme_items, 10, Arc::clone(&list_theme), None);
        theme_list.set_selected_index(theme_index);
        let analytics_list = SelectList::new(analytics_items, 10, list_theme, None);

        Self {
            step: SetupStep::Theme,
            theme,
            themes,
            theme_index,
            analytics_index: 0,
            detected_theme,
            theme_list,
            analytics_list,
            on_submit: Some(on_submit),
            on_cancel: Some(on_cancel),
            on_theme_preview: None,
            focused: false,
        }
    }

    /// Swap the render theme (setup theme preview, startup-ui.ts
    /// `onThemePreview` → `setTheme`): the select lists are rebuilt with the
    /// new theme, preserving the current selections and step.
    pub(crate) fn set_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme;
        let list_theme = Arc::new(select_list_theme(&self.theme));
        let mut theme_list = SelectList::new(
            self.themes
                .iter()
                .map(|name| SelectItem {
                    value: name.clone(),
                    label: name.clone(),
                    description: None,
                })
                .collect(),
            10,
            Arc::clone(&list_theme),
            None,
        );
        theme_list.set_selected_index(self.theme_index);
        let mut analytics_list = SelectList::new(
            ANALYTICS_OPTIONS
                .iter()
                .map(|(value, label)| SelectItem {
                    value: (*value).to_string(),
                    label: (*label).to_string(),
                    description: None,
                })
                .collect(),
            10,
            list_theme,
            None,
        );
        analytics_list.set_selected_index(self.analytics_index);
        self.theme_list = theme_list;
        self.analytics_list = analytics_list;
    }

    /// The currently selected theme name.
    pub fn selected_theme(&self) -> Option<&str> {
        self.themes.get(self.theme_index).map(String::as_str)
    }

    /// The currently selected analytics value (true = opt in).
    pub fn selected_analytics(&self) -> bool {
        self.analytics_index == 0
    }

    /// `moveSelection` (first-time-setup.ts:112-123): clamp-move the active
    /// step's selection; theme changes fire `on_theme_preview`.
    fn move_selection(&mut self, delta: i64) {
        match self.step {
            SetupStep::Theme => {
                let len = self.themes.len() as i64;
                let next = (self.theme_index as i64 + delta).clamp(0, (len - 1).max(0));
                if next != self.theme_index as i64 {
                    self.theme_index = next as usize;
                    if let Some(on_theme_preview) = self.on_theme_preview.as_mut() {
                        if let Some(name) = self.themes.get(self.theme_index) {
                            on_theme_preview(name);
                        }
                    }
                }
            }
            SetupStep::Analytics => {
                self.analytics_index = (self.analytics_index as i64 + delta).clamp(0, 1) as usize;
            }
        }
        self.theme_list.set_selected_index(self.theme_index);
        self.analytics_list.set_selected_index(self.analytics_index);
    }

    fn border_line(&self, width: usize) -> String {
        let theme = Arc::clone(&self.theme);
        DynamicBorder::new(Box::new(move |s: &str| theme.fg("border", s)))
            .render(width)
            .pop()
            .unwrap_or_default()
    }
}

impl Component for FirstTimeSetupComponent {
    /// `update` (first-time-setup.ts:49-101): rebuilds the whole dialog on
    /// every render so theme previews recolor all text — border, logo,
    /// welcome, step content (SelectList), hint, border.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(self.border_line(width)); // DynamicBorder
        lines.push(String::new()); // Spacer(1)
        lines.extend(
            Text::new(
                self.theme.fg("accent", &SETUP_LOGO_LINES.join("\n")),
                1,
                0,
                None,
            )
            .render(width),
        );
        lines.push(String::new()); // Spacer(1)
        let welcome = format!(
            "Welcome to {}, the minimal coding agent.",
            crate::config::APP_NAME
        );
        lines.extend(
            Text::new(self.theme.fg("accent", &Theme::bold(&welcome)), 1, 0, None).render(width),
        );
        lines.push(String::new()); // Spacer(1)

        match self.step {
            SetupStep::Theme => {
                lines.extend(
                    Text::new(self.theme.fg("text", "Pick a theme."), 1, 0, None).render(width),
                );
                if let Some(detected) = &self.detected_theme {
                    lines.extend(
                        Text::new(
                            self.theme
                                .fg("muted", &format!("Detected system appearance: {detected}")),
                            1,
                            0,
                            None,
                        )
                        .render(width),
                    );
                }
                lines.push(String::new()); // Spacer(1)
                lines.extend(self.theme_list.render(width));
            }
            SetupStep::Analytics => {
                lines.extend(
                    Text::new(
                        self.theme
                            .fg("text", "Opt-in to anonymous usage data sharing?"),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
                lines.extend(
                    Text::new(
                        self.theme.fg(
                            "muted",
                            "Opting in stores a tracking identifier in settings.json and enables anonymous\nusage analytics. This helps us to better debug, reproduce, and resolve issues\nand bugs within pir. You can observe what is shared using /privacy and make\nchanges anytime in settings.json.",
                        ),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
                lines.push(String::new()); // Spacer(1)
                lines.extend(self.analytics_list.render(width));
            }
        }

        lines.push(String::new()); // Spacer(1)
                                   // Key hints (first-time-setup.ts:88-98).
        let hint = format!(
            "{}  {}  {}",
            raw_key_hint(&self.theme, "↑↓", "navigate"),
            key_hint(
                &self.theme,
                "tui.select.confirm",
                if self.step == SetupStep::Theme {
                    "continue"
                } else {
                    "finish"
                },
            ),
            key_hint(&self.theme, "tui.select.cancel", "skip setup"),
        );
        lines.extend(Text::new(hint, 1, 0, None).render(width));
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines
    }

    /// `handleInput` (first-time-setup.ts:125-144): up/down (with literal
    /// `k`/`j`), confirm (with literal `\n`) advances/submits, cancel skips
    /// the setup.
    fn handle_input(&mut self, data: &str) {
        let kb = pir_tui::keybindings::get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if read.matches_id(data, "tui.select.up") || data == "k" {
            self.move_selection(-1);
        } else if read.matches_id(data, "tui.select.down") || data == "j" {
            self.move_selection(1);
        } else if read.matches_id(data, "tui.select.confirm") || data == "\n" {
            if self.step == SetupStep::Theme {
                self.step = SetupStep::Analytics;
                self.move_selection(0); // keep the lists' marks in sync
            } else {
                let result = FirstTimeSetupResult {
                    theme: self
                        .themes
                        .get(self.theme_index)
                        .cloned()
                        .unwrap_or_default(),
                    analytics: self.analytics_index == 0,
                };
                if let Some(on_submit) = self.on_submit.as_mut() {
                    on_submit(result);
                }
            }
        } else if read.matches_id(data, "tui.select.cancel") {
            if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
        }
    }

    fn invalidate(&mut self) {
        self.theme_list.invalidate();
        self.analytics_list.invalidate();
    }
}

impl Focusable for FirstTimeSetupComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
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
    }

    struct Captures {
        submitted: Arc<Mutex<Option<(String, bool)>>>,
        cancelled: Arc<Mutex<u32>>,
        previewed: Arc<Mutex<Vec<String>>>,
    }

    fn component_with(detected: Option<&str>) -> (FirstTimeSetupComponent, Captures) {
        let submitted = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(Mutex::new(0u32));
        let previewed = Arc::new(Mutex::new(Vec::new()));
        let submitted_thread = Arc::clone(&submitted);
        let cancelled_thread = Arc::clone(&cancelled);
        let previewed_thread = Arc::clone(&previewed);
        let mut component = FirstTimeSetupComponent::new(
            theme(),
            vec!["dark".to_string(), "light".to_string()],
            detected.map(str::to_string),
            Box::new(move |result: FirstTimeSetupResult| {
                *submitted_thread.lock().unwrap() = Some((result.theme, result.analytics));
            }),
            Box::new(move || {
                *cancelled_thread.lock().unwrap() += 1;
            }),
        );
        component.on_theme_preview = Some(Box::new(move |name: &str| {
            previewed_thread.lock().unwrap().push(name.to_string());
        }));
        (
            component,
            Captures {
                submitted,
                cancelled,
                previewed,
            },
        )
    }

    #[test]
    fn detected_theme_is_preselected_and_renders_step_one() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _) = component_with(Some("light"));
        assert_eq!(component.selected_theme(), Some("light"));

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains('─')), "borders rendered");
        assert!(lines.iter().any(|l| l.contains("████")), "logo rendered");
        assert!(lines
            .iter()
            .any(|l| l.contains("Welcome to pir, the minimal coding agent.")));
        assert!(lines.iter().any(|l| l.contains("Pick a theme.")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Detected system appearance: light")));
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("→ light")),
            "detected preselected"
        );
        assert!(lines.iter().any(|l| l.contains("continue")));
        assert!(lines.iter().any(|l| l.contains("skip setup")));
    }

    #[test]
    fn unknown_detected_theme_falls_back_to_first() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _) = component_with(Some("nope"));
        assert_eq!(component.selected_theme(), Some("dark"));
    }

    #[test]
    fn no_detected_theme_skips_detected_line() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _) = component_with(None);
        assert_eq!(component.selected_theme(), Some("dark"));
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(!lines
            .iter()
            .any(|l| l.contains("Detected system appearance")));
    }

    #[test]
    fn confirm_advances_to_analytics_then_submits_result() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("light"));

        component.handle_input("\r"); // step 1 → step 2
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(!lines.iter().any(|l| l.contains("Pick a theme.")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Opt-in to anonymous usage data sharing?")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Share anonymous usage data")));
        assert!(lines.iter().any(|l| l.contains("Don't share")));
        assert!(lines.iter().any(|l| l.contains("finish")));
        assert!(lines.iter().any(|l| l.contains("tracking identifier")));

        component.handle_input("\r"); // step 2 → submit (default: analytics on)
        assert_eq!(
            *captures.submitted.lock().unwrap(),
            Some(("light".to_string(), true))
        );
        assert_eq!(*captures.cancelled.lock().unwrap(), 0);
    }

    #[test]
    fn jk_navigation_moves_theme_and_previews() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);
        assert_eq!(component.selected_theme(), Some("dark"));

        component.handle_input("j"); // literal j = down
        assert_eq!(component.selected_theme(), Some("light"));
        assert_eq!(
            *captures.previewed.lock().unwrap(),
            vec!["light".to_string()],
            "theme change fires the preview hook"
        );
        component.handle_input("j"); // clamped
        assert_eq!(component.selected_theme(), Some("light"));
        component.handle_input("k"); // literal k = up
        assert_eq!(component.selected_theme(), Some("dark"));
        assert_eq!(captures.previewed.lock().unwrap().len(), 2);

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.trim_start().starts_with("→ dark")));
    }

    #[test]
    fn analytics_selection_controls_result_flag() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("dark"));

        component.handle_input("\r"); // to analytics
        component.handle_input("\x1b[B"); // down → "Don't share"
        assert!(!component.selected_analytics());
        component.handle_input("\r"); // submit
        assert_eq!(
            *captures.submitted.lock().unwrap(),
            Some(("dark".to_string(), false))
        );
    }

    #[test]
    fn escape_skips_setup_from_either_step() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        component.handle_input("\x1b");
        assert_eq!(*captures.cancelled.lock().unwrap(), 1);
        assert_eq!(captures.submitted.lock().unwrap().as_ref(), None);

        component.handle_input("\r"); // to analytics
        component.handle_input("\x03"); // ctrl+c also cancels
        assert_eq!(*captures.cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn literal_newline_confirms() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("light"));

        component.handle_input("\n"); // literal newline → analytics step
        component.handle_input("\n"); // submit
        assert_eq!(
            *captures.submitted.lock().unwrap(),
            Some(("light".to_string(), true))
        );
    }

    #[test]
    fn set_theme_swaps_render_theme_and_preserves_selections() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _) = component_with(None); // dark preselected
        component.handle_input("j"); // theme → light
        component.handle_input("\r"); // analytics step

        let light_theme = Arc::new(crate::core::themes::load_theme("light", None).unwrap());
        component.set_theme(light_theme);

        // Selections survived the rebuild.
        assert_eq!(component.selected_theme(), Some("light"));
        assert!(component.selected_analytics());
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines
            .iter()
            .any(|l| l.contains("Opt-in to anonymous usage data sharing?")));
    }
}
