//! Port of `extension_selector.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - The title is `Option<String>` — a local extension: `None` renders a
//!   plain selector without the accent title row (and without the countdown
//!   suffix); upstream always requires a title.
//! - Upstream `opts.tui` (a `TUI` instance) becomes
//!   `render_handle: Option<RenderHandle>` — the only capability the
//!   [`CountdownTimer`] needs (`tui.requestRender()` per tick); the timeout
//!   is `timeout_ms: Option<u64>` (upstream `timeout` in ms).
//! - Callback shape: upstream has `onSelect(option: string)` and
//!   `onCancel()`; the port merges them into
//!   `on_select: Box<dyn FnMut(Option<String>) + Send>` — `Some(option)` on
//!   confirm, `None` on cancel and on timeout. The separate `on_cancel`
//!   callback is retained and fires on cancel **and** timeout, exactly where
//!   upstream invokes `onCancelCallback` (extension-selector.ts:56, 104-105);
//!   integration code should subscribe to one of the two.
//! - The literal `k` / `j` / `\n` key fallbacks are kept
//!   (extension-selector.ts:95-101); `app.tools.expand` (Ctrl+O) matching goes
//!   through the installed global keybinding manager (which carries the
//!   `app.*` ids, installed by `install_global_keybindings`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, RenderHandle};

use crate::core::themes::Theme;

use super::countdown_timer::CountdownTimer;
use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{key_hint, raw_key_hint};

/// `ExtensionSelectorOptions` (extension-selector.ts:12-16): `tui` →
/// [`RenderHandle`], `timeout` → milliseconds.
#[derive(Default)]
pub struct ExtensionSelectorOptions {
    /// Render request capability for the countdown ticks (upstream `tui`).
    pub render_handle: Option<RenderHandle>,
    /// Timeout in milliseconds; `Some(n > 0)` arms the countdown.
    pub timeout_ms: Option<u64>,
    /// Called when the user presses Ctrl+O (`app.tools.expand`,
    /// extension-selector.ts:93-94).
    pub on_toggle_tools_expanded: Option<Box<dyn FnMut() + Send>>,
}

/// `ExtensionSelectorComponent` (extension-selector.ts:18-111): a bordered
/// list of string options with up/down + `k`/`j` navigation, an optional
/// countdown in the title (`Title (Ns)`), a Ctrl+O toggle hook and a key
/// hint footer.
pub struct ExtensionSelectorComponent {
    theme: Arc<Theme>,
    title: Option<String>,
    options: Vec<String>,
    selected_index: usize,
    /// Remaining seconds shown in the title while the countdown is armed;
    /// 0 when no countdown is configured (extension-selector.ts:52-58).
    remaining_seconds: Arc<AtomicU64>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_select: Arc<Mutex<Option<Box<dyn FnMut(Option<String>) + Send>>>>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_cancel: Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>,
    on_toggle_tools_expanded: Option<Box<dyn FnMut() + Send>>,
    countdown: Option<CountdownTimer>,
}

impl ExtensionSelectorComponent {
    /// `constructor` (extension-selector.ts:29-78).
    pub fn new(
        theme: Arc<Theme>,
        title: Option<String>,
        options: Vec<String>,
        on_select: Box<dyn FnMut(Option<String>) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        opts: Option<ExtensionSelectorOptions>,
    ) -> Self {
        let opts = opts.unwrap_or_default();
        let remaining_seconds = Arc::new(AtomicU64::new(0));
        let mut component = Self {
            theme,
            title,
            options,
            selected_index: 0,
            remaining_seconds: Arc::clone(&remaining_seconds),
            on_select: Arc::new(Mutex::new(Some(on_select))),
            on_cancel: Arc::new(Mutex::new(Some(on_cancel))),
            on_toggle_tools_expanded: opts.on_toggle_tools_expanded,
            countdown: None,
        };
        if let (Some(render_handle), Some(timeout_ms)) = (opts.render_handle, opts.timeout_ms) {
            if timeout_ms > 0 {
                component.countdown = Some(component.start_countdown(timeout_ms, render_handle));
            }
        }
        component
    }

    /// Start the countdown (extension-selector.ts:51-58). The timer fires the
    /// initial tick synchronously, so the title shows the full remaining
    /// seconds immediately.
    fn start_countdown(&mut self, timeout_ms: u64, render_handle: RenderHandle) -> CountdownTimer {
        let seconds = Arc::clone(&self.remaining_seconds);
        let on_select = Arc::clone(&self.on_select);
        let on_cancel = Arc::clone(&self.on_cancel);
        CountdownTimer::new(
            timeout_ms,
            Some(render_handle),
            Box::new(move |remaining| {
                seconds.store(remaining, Ordering::SeqCst);
            }),
            Box::new(move || {
                if let Some(on_cancel) = on_cancel
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
                {
                    on_cancel();
                }
                if let Some(on_select) = on_select
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
                {
                    on_select(None);
                }
            }),
        )
    }

    /// The currently selected option (test/integration helper).
    pub fn selected_option(&self) -> Option<&str> {
        self.options.get(self.selected_index).map(String::as_str)
    }

    /// Remaining seconds of the countdown, or 0 when none is armed.
    pub fn remaining_seconds(&self) -> u64 {
        self.remaining_seconds.load(Ordering::SeqCst)
    }

    /// `dispose` (extension-selector.ts:109-111): stop the countdown thread.
    pub fn dispose(&mut self) {
        if let Some(countdown) = self.countdown.as_mut() {
            countdown.dispose();
        }
    }

    /// Cancel path shared by the cancel key and the countdown expiry
    /// (extension-selector.ts:56, 104-105): both callbacks fire.
    fn fire_cancel(&mut self) {
        if let Some(on_cancel) = self
            .on_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            on_cancel();
        }
        if let Some(on_select) = self
            .on_select
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            on_select(None);
        }
    }

    fn border_line(&self, width: usize) -> String {
        let theme = Arc::clone(&self.theme);
        DynamicBorder::new(Box::new(move |s: &str| theme.fg("border", s)))
            .render(width)
            .pop()
            .unwrap_or_default()
    }
}

impl Component for ExtensionSelectorComponent {
    /// Container layout (extension-selector.ts:44-75): border, title (with
    /// countdown suffix), option list, hint, border.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(self.border_line(width)); // DynamicBorder
        lines.push(String::new()); // Spacer(1)

        if let Some(title) = &self.title {
            let title_text = if self.countdown.is_some() {
                format!(
                    "{title} ({}s)",
                    self.remaining_seconds.load(Ordering::SeqCst)
                )
            } else {
                title.clone()
            };
            // Title with countdown (extension-selector.ts:47, 52-57).
            lines.extend(
                Text::new(
                    self.theme.fg("accent", &Theme::bold(&title_text)),
                    1,
                    0,
                    None,
                )
                .render(width),
            );
            lines.push(String::new()); // Spacer(1)
        }

        // Option list (extension-selector.ts:80-89).
        for (i, option) in self.options.iter().enumerate() {
            let is_selected = i == self.selected_index;
            let text = if is_selected {
                format!(
                    "{}{}",
                    self.theme.fg("accent", "→ "),
                    self.theme.fg("accent", option)
                )
            } else {
                format!("  {}", self.theme.fg("text", option))
            };
            lines.extend(Text::new(text, 1, 0, None).render(width));
        }

        lines.push(String::new()); // Spacer(1)
                                   // Key hints (extension-selector.ts:63-73).
        let hint = format!(
            "{}  {}  {}",
            raw_key_hint(&self.theme, "↑↓", "navigate"),
            key_hint(&self.theme, "tui.select.confirm", "select"),
            key_hint(&self.theme, "tui.select.cancel", "cancel"),
        );
        lines.extend(Text::new(hint, 1, 0, None).render(width));
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines
    }

    /// `handleInput` (extension-selector.ts:91-107): Ctrl+O toggle, up/down
    /// (with literal `k`/`j`), confirm (with literal `\n`), cancel.
    fn handle_input(&mut self, data: &str) {
        let kb = rpi_tui::keybindings::get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if read.matches_id(data, "app.tools.expand") {
            if let Some(on_toggle) = self.on_toggle_tools_expanded.as_mut() {
                on_toggle();
            }
        } else if read.matches_id(data, "tui.select.up") || data == "k" {
            self.selected_index = self.selected_index.saturating_sub(1);
        } else if read.matches_id(data, "tui.select.down") || data == "j" {
            self.selected_index =
                (self.selected_index + 1).min(self.options.len().saturating_sub(1));
        } else if read.matches_id(data, "tui.select.confirm") || data == "\n" {
            if let Some(option) = self.options.get(self.selected_index) {
                if let Some(on_select) = self
                    .on_select
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
                {
                    on_select(Some(option.clone()));
                }
            }
        } else if read.matches_id(data, "tui.select.cancel") {
            self.fire_cancel();
        }
    }

    fn invalidate(&mut self) {
        // No cached state to invalidate currently.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

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
        selected: Arc<Mutex<Option<String>>>,
        cancelled: Arc<Mutex<u32>>,
        toggled: Arc<Mutex<u32>>,
    }

    fn component_with(
        title: Option<&str>,
        options: Vec<&str>,
        opts: Option<ExtensionSelectorOptions>,
    ) -> (ExtensionSelectorComponent, Captures) {
        let selected = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(Mutex::new(0u32));
        let toggled = Arc::new(Mutex::new(0u32));
        let selected_thread = Arc::clone(&selected);
        let cancelled_thread = Arc::clone(&cancelled);
        let component = ExtensionSelectorComponent::new(
            theme(),
            title.map(str::to_string),
            options.into_iter().map(str::to_string).collect(),
            Box::new(move |option: Option<String>| {
                *selected_thread.lock().unwrap() = option;
            }),
            Box::new(move || {
                *cancelled_thread.lock().unwrap() += 1;
            }),
            opts,
        );
        // Toggle hook needs &mut, so it is installed after construction.
        let mut component = component;
        let toggle_capture = Arc::clone(&toggled);
        component.on_toggle_tools_expanded = Some(Box::new(move || {
            *toggle_capture.lock().unwrap() += 1;
        }));
        (
            component,
            Captures {
                selected,
                cancelled,
                toggled,
            },
        )
    }

    #[test]
    fn up_down_and_jk_navigate_with_clamping() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _) = component_with(Some("Title"), vec!["a", "b", "c"], None);

        assert_eq!(component.selected_option(), Some("a"));
        component.handle_input("\x1b[B"); // down
        assert_eq!(component.selected_option(), Some("b"));
        component.handle_input("j"); // literal j = down
        assert_eq!(component.selected_option(), Some("c"));
        component.handle_input("\x1b[B"); // clamped at the end
        assert_eq!(component.selected_option(), Some("c"));
        component.handle_input("k"); // literal k = up
        assert_eq!(component.selected_option(), Some("b"));
        component.handle_input("\x1b[A"); // up
        assert_eq!(component.selected_option(), Some("a"));
        component.handle_input("\x1b[A"); // clamped at the start
        assert_eq!(component.selected_option(), Some("a"));

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.contains("→ a")),
            "selected row marked"
        );
        assert!(lines.iter().any(|l| l.contains("Title")), "title rendered");
        assert!(lines.iter().any(|l| l.contains("navigate")));
        assert!(lines.iter().any(|l| l.contains("select")));
        assert!(lines.iter().any(|l| l.contains("cancel")));
        assert!(lines.iter().any(|l| l.contains('─')), "borders rendered");
    }

    #[test]
    fn confirm_selects_and_literal_newline_confirms() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("Title"), vec!["a", "b"], None);

        component.handle_input("\x1b[B");
        component.handle_input("\r"); // tui.select.confirm
        assert_eq!(captures.selected.lock().unwrap().as_deref(), Some("b"));

        component.handle_input("j"); // wrap down (clamped)
        component.handle_input("\n"); // literal newline confirm
        assert_eq!(captures.selected.lock().unwrap().as_deref(), Some("b"));
    }

    #[test]
    fn cancel_key_fires_on_cancel_and_on_select_none() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("Title"), vec!["a", "b"], None);

        component.handle_input("\x1b");
        assert_eq!(*captures.cancelled.lock().unwrap(), 1);
        assert_eq!(captures.selected.lock().unwrap().as_deref(), None);

        component.handle_input("\x03"); // ctrl+c also cancels
        assert_eq!(*captures.cancelled.lock().unwrap(), 2);
        assert_eq!(captures.selected.lock().unwrap().as_deref(), None);
    }

    #[test]
    fn ctrl_o_fires_toggle_hook() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("Title"), vec!["a"], None);

        component.handle_input("\x0f"); // ctrl+o = app.tools.expand
        component.handle_input("\x0f");
        assert_eq!(*captures.toggled.lock().unwrap(), 2);
        // Non-matching keys don't toggle.
        component.handle_input("\x18"); // ctrl+x
        assert_eq!(*captures.toggled.lock().unwrap(), 2);
    }

    #[test]
    fn countdown_shows_seconds_and_expires_into_cancel() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let opts = ExtensionSelectorOptions {
            render_handle: Some(RenderHandle::new(|| {})),
            timeout_ms: Some(500),
            ..Default::default()
        };
        let (mut component, captures) = component_with(Some("Title"), vec!["a"], Some(opts));

        // Initial tick fires synchronously: ceil(500/1000) = 1s.
        assert_eq!(component.remaining_seconds(), 1);
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.contains("Title (1s)")),
            "countdown in title"
        );

        // Expiry (~1s later) fires on_cancel + on_select(None).
        let deadline = Instant::now() + Duration::from_secs(3);
        while *captures.cancelled.lock().unwrap() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            *captures.cancelled.lock().unwrap(),
            1,
            "timeout must cancel"
        );
        assert_eq!(captures.selected.lock().unwrap().as_deref(), None);
        assert_eq!(component.remaining_seconds(), 0);
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("Title (0s)")));
        component.dispose();
    }

    #[test]
    fn no_countdown_without_timeout() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("Title"), vec!["a"], None);

        assert_eq!(component.remaining_seconds(), 0);
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("Title")), "plain title");
        assert!(
            !lines.iter().any(|l| l.contains("Title (")),
            "no countdown suffix"
        );

        component.handle_input("\x1b");
        assert_eq!(*captures.cancelled.lock().unwrap(), 1);
        assert_eq!(captures.selected.lock().unwrap().as_deref(), None);
    }

    #[test]
    fn optional_title_skips_title_row() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _) = component_with(None, vec!["a"], None);
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.contains(" a") || l.contains("→ a")),
            "option listed"
        );
        assert!(!lines.iter().any(|l| l.contains("Title")));
    }

    #[test]
    fn empty_options_render_without_panicking() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _) = component_with(Some("Title"), Vec::<&str>::new(), None);
        component.handle_input("\r");
        component.handle_input("\x1b");
        assert!(!component.render(80).is_empty());
    }
}
