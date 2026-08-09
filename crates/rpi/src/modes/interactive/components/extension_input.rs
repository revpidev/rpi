//! Port of `extension_input.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - Upstream `opts.tui` (a `TUI` instance) becomes
//!   `render_handle: Option<RenderHandle>` and the timeout is
//!   `timeout_ms: Option<u64>` (upstream `timeout` in ms) — the same
//!   countdown pattern as [`super::extension_selector`].
//! - The `placeholder` constructor parameter is kept for signature parity but
//!   ignored, exactly like upstream (`_placeholder`, extension-input.ts:36).
//! - The literal `\n` confirm fallback is kept (extension-input.ts:75).
//! - Callbacks are `Box<dyn FnMut ... + Send>` fields; the cancel callback is
//!   shared with the countdown expiry thread via `Arc<Mutex<...>>`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_tui::components::input::Input;
use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, Focusable, RenderHandle};

use crate::core::themes::Theme;

use super::countdown_timer::CountdownTimer;
use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_hint;

/// `ExtensionInputOptions` (extension-input.ts:11-14): `tui` →
/// [`RenderHandle`], `timeout` → milliseconds.
#[derive(Default)]
pub struct ExtensionInputOptions {
    /// Render request capability for the countdown ticks (upstream `tui`).
    pub render_handle: Option<RenderHandle>,
    /// Timeout in milliseconds; `Some(n > 0)` arms the countdown.
    pub timeout_ms: Option<u64>,
}

/// `ExtensionInputComponent` (extension-input.ts:16-86): a bordered single
/// line input with an optional countdown in the title (`Title (Ns)`) and a
/// key hint footer. Enter submits the input value, Escape/ctrl+c cancels,
/// everything else is forwarded to the inner [`Input`].
pub struct ExtensionInputComponent {
    input: Input,
    theme: Arc<Theme>,
    base_title: String,
    /// Remaining seconds shown in the title while the countdown is armed;
    /// 0 when no countdown is configured (extension-input.ts:54-61).
    remaining_seconds: Arc<AtomicU64>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_submit: Option<Box<dyn FnMut(&str) + Send>>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_cancel: Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>,
    countdown: Option<CountdownTimer>,
    focused: bool,
}

impl ExtensionInputComponent {
    /// `constructor` (extension-input.ts:34-71). `placeholder` is ignored
    /// like upstream.
    pub fn new(
        theme: Arc<Theme>,
        title: String,
        _placeholder: Option<String>,
        on_submit: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        opts: Option<ExtensionInputOptions>,
    ) -> Self {
        let opts = opts.unwrap_or_default();
        let remaining_seconds = Arc::new(AtomicU64::new(0));
        let mut component = Self {
            input: Input::new(),
            theme,
            base_title: title,
            remaining_seconds: Arc::clone(&remaining_seconds),
            on_submit: Some(on_submit),
            on_cancel: Arc::new(Mutex::new(Some(on_cancel))),
            countdown: None,
            focused: false,
        };
        if let (Some(render_handle), Some(timeout_ms)) = (opts.render_handle, opts.timeout_ms) {
            if timeout_ms > 0 {
                component.countdown = Some(component.start_countdown(timeout_ms, render_handle));
            }
        }
        component
    }

    /// Start the countdown (extension-input.ts:54-61). The timer fires the
    /// initial tick synchronously, so the title shows the full remaining
    /// seconds immediately.
    fn start_countdown(&mut self, timeout_ms: u64, render_handle: RenderHandle) -> CountdownTimer {
        let seconds = Arc::clone(&self.remaining_seconds);
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
            }),
        )
    }

    /// Remaining seconds of the countdown, or 0 when none is armed.
    pub fn remaining_seconds(&self) -> u64 {
        self.remaining_seconds.load(Ordering::SeqCst)
    }

    /// `dispose` (extension-input.ts:84-86): stop the countdown thread.
    pub fn dispose(&mut self) {
        if let Some(countdown) = self.countdown.as_mut() {
            countdown.dispose();
        }
    }

    /// The current input value (test/integration helper).
    pub fn value(&self) -> &str {
        self.input.get_value()
    }

    fn cancel(&mut self) {
        if let Some(on_cancel) = self
            .on_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            on_cancel();
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

impl Component for ExtensionInputComponent {
    /// Container layout (extension-input.ts:47-70): border, title (with
    /// countdown suffix), input, hint, border.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(self.border_line(width)); // DynamicBorder
        lines.push(String::new()); // Spacer(1)

        let title_text = if self.countdown.is_some() {
            format!(
                "{} ({}s)",
                self.base_title,
                self.remaining_seconds.load(Ordering::SeqCst)
            )
        } else {
            self.base_title.clone()
        };
        // Title with countdown (extension-input.ts:50, 54-60).
        lines.extend(Text::new(self.theme.fg("accent", &title_text), 1, 0, None).render(width));
        lines.push(String::new()); // Spacer(1)

        lines.extend(self.input.render(width));

        lines.push(String::new()); // Spacer(1)
                                   // Key hints (extension-input.ts:66-68).
        let hint = format!(
            "{}  {}",
            key_hint(&self.theme, "tui.select.confirm", "submit"),
            key_hint(&self.theme, "tui.select.cancel", "cancel"),
        );
        lines.extend(Text::new(hint, 1, 0, None).render(width));
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines
    }

    /// `handleInput` (extension-input.ts:73-82): confirm (with literal `\n`),
    /// cancel, else forward to the input.
    fn handle_input(&mut self, data: &str) {
        let kb = rpi_tui::keybindings::get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if read.matches_id(data, "tui.select.confirm") || data == "\n" {
            let value = self.input.get_value().to_string();
            if let Some(on_submit) = self.on_submit.as_mut() {
                on_submit(&value);
            }
        } else if read.matches_id(data, "tui.select.cancel") {
            self.cancel();
        } else {
            drop(read);
            self.input.handle_input(data);
        }
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ExtensionInputComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    /// Propagate focus to the inner input for IME cursor positioning
    /// (extension-input.ts:24-32).
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        submitted: Arc<Mutex<Option<String>>>,
        cancelled: Arc<Mutex<u32>>,
    }

    fn component_with(opts: Option<ExtensionInputOptions>) -> (ExtensionInputComponent, Captures) {
        let submitted = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(Mutex::new(0u32));
        let submitted_thread = Arc::clone(&submitted);
        let cancelled_thread = Arc::clone(&cancelled);
        let component = ExtensionInputComponent::new(
            theme(),
            "Extension Input".to_string(),
            None,
            Box::new(move |value: &str| {
                *submitted_thread.lock().unwrap() = Some(value.to_string());
            }),
            Box::new(move || {
                *cancelled_thread.lock().unwrap() += 1;
            }),
            opts,
        );
        (
            component,
            Captures {
                submitted,
                cancelled,
            },
        )
    }

    #[test]
    fn typing_forwards_to_input_and_enter_submits() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        for ch in "hello".chars() {
            component.handle_input(&ch.to_string());
        }
        assert_eq!(component.value(), "hello");

        component.handle_input("\r"); // tui.select.confirm
        assert_eq!(captures.submitted.lock().unwrap().as_deref(), Some("hello"));
        assert_eq!(*captures.cancelled.lock().unwrap(), 0);
    }

    #[test]
    fn literal_newline_confirms_and_empty_value_submits_empty() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        component.handle_input("\n"); // literal newline confirm
        assert_eq!(captures.submitted.lock().unwrap().as_deref(), Some(""));
    }

    #[test]
    fn escape_and_ctrl_c_cancel_without_submitting() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        component.handle_input("abc");
        component.handle_input("\x1b");
        assert_eq!(*captures.cancelled.lock().unwrap(), 1);
        assert_eq!(captures.submitted.lock().unwrap().as_deref(), None);

        component.handle_input("\x03"); // ctrl+c also cancels
        assert_eq!(*captures.cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn renders_layout_title_and_hints() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _) = component_with(None);
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("Extension Input")));
        assert!(
            lines.iter().any(|l| l.contains("> ")),
            "input prompt rendered"
        );
        assert!(lines.iter().any(|l| l.contains("submit")));
        assert!(lines.iter().any(|l| l.contains("cancel")));
        assert!(lines.iter().any(|l| l.contains('─')), "borders rendered");
        assert!(
            lines.iter().any(|l| l.trim_end().is_empty()),
            "spacers rendered"
        );
    }

    #[test]
    fn countdown_shows_seconds_and_expires_into_cancel() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let opts = ExtensionInputOptions {
            render_handle: Some(RenderHandle::new(|| {})),
            timeout_ms: Some(500),
        };
        let (mut component, captures) = component_with(Some(opts));

        // Initial tick fires synchronously: ceil(500/1000) = 1s.
        assert_eq!(component.remaining_seconds(), 1);
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("Extension Input (1s)")));

        let deadline = Instant::now() + Duration::from_secs(3);
        while *captures.cancelled.lock().unwrap() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            *captures.cancelled.lock().unwrap(),
            1,
            "timeout must cancel"
        );
        assert_eq!(component.remaining_seconds(), 0);
        component.dispose();
    }

    #[test]
    fn focus_propagates_to_inner_input() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _) = component_with(None);
        assert!(!component.focused());
        assert!(!component.input.focused());
        component.set_focused(true);
        assert!(component.focused());
        assert!(component.input.focused(), "inner input must receive focus");
        component.set_focused(false);
        assert!(!component.input.focused());
    }
}
