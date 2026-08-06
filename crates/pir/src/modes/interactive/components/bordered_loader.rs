//! Port of `bordered_loader.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816); the loader gets a
//!   [`RenderHandle`] instead of a `TUI` instance (loader.ts:88-89).
//! - Upstream uses [`CancellableLoader`] + a DOM `AbortController`/`AbortSignal`
//!   for the cancellable branch and an unused `AbortController` for the
//!   non-cancellable one (bordered-loader.ts:17-32); the port always uses
//!   [`Loader`] (so [`BorderedLoaderComponent::set_message`] can update the
//!   text — `CancellableLoader` hides its inner `Loader`) and implements the
//!   escape-abort itself: a `bool` flag + the `on_abort` field. The upstream
//!   `signal` getter (bordered-loader.ts:42-47) is replaced by
//!   [`BorderedLoaderComponent::aborted`]; the non-cancellable branch's dummy
//!   never-aborting signal is dropped.
//! - `set_message` is a local addition — upstream has no message setter.
//! - `handleInput` forwarding (bordered-loader.ts:55-59) is kept: only the
//!   cancellable variant reacts, and `on_abort` fires on every matching
//!   cancel keypress like upstream's `CancellableLoader` (cancellable-loader.ts:29-35).

use std::sync::Arc;

use pir_tui::components::loader::Loader;
use pir_tui::components::text::Text;
use pir_tui::tui::{Component, RenderHandle};

use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_hint;

/// Loader options (upstream `{ cancellable?: boolean }`,
/// bordered-loader.ts:12). `cancellable` defaults to `true`.
pub struct BorderedLoaderOptions {
    pub cancellable: bool,
}

/// `BorderedLoader` (bordered-loader.ts:7-67): a [`Loader`] wrapped in
/// `DynamicBorder`s, with an optional "cancel" hint and Escape-to-abort when
/// cancellable.
pub struct BorderedLoaderComponent {
    loader: Loader,
    theme: Arc<Theme>,
    cancellable: bool,
    aborted: bool,
    /// Called when the user presses Escape/Ctrl+C while cancellable
    /// (upstream `onAbort`, bordered-loader.ts:49-53).
    pub on_abort: Option<Box<dyn FnMut() + Send>>,
}

impl BorderedLoaderComponent {
    /// `constructor` (bordered-loader.ts:12-40).
    pub fn new(
        render_handle: RenderHandle,
        theme: Arc<Theme>,
        message: impl Into<String>,
        options: Option<BorderedLoaderOptions>,
    ) -> Self {
        let cancellable = options.map(|options| options.cancellable).unwrap_or(true);
        let spinner_theme = Arc::clone(&theme);
        let message_theme = Arc::clone(&theme);
        let loader = Loader::new(
            render_handle,
            move |s: &str| spinner_theme.fg("accent", s),
            move |s: &str| message_theme.fg("muted", s),
            message,
            None,
        );
        Self {
            loader,
            theme,
            cancellable,
            aborted: false,
            on_abort: None,
        }
    }

    /// Whether Escape-abort is armed (upstream `cancellable`).
    pub fn is_cancellable(&self) -> bool {
        self.cancellable
    }

    /// Whether the user aborted with Escape/Ctrl+C (replaces the upstream
    /// `signal.aborted`, see header note).
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// Update the loader message (local addition — see header note).
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.loader.set_message(message);
    }

    /// `dispose` (bordered-loader.ts:61-67): stop the animation thread.
    pub fn dispose(&mut self) {
        self.loader.stop();
    }

    fn border_line(&self, width: usize) -> String {
        let theme = Arc::clone(&self.theme);
        DynamicBorder::new(Box::new(move |s: &str| theme.fg("border", s)))
            .render(width)
            .pop()
            .unwrap_or_default()
    }
}

impl Component for BorderedLoaderComponent {
    /// Container layout (bordered-loader.ts:16-40): border, loader, optional
    /// cancel hint, border.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(self.border_line(width)); // DynamicBorder
        lines.extend(self.loader.render(width));
        if self.cancellable {
            lines.push(String::new()); // Spacer(1)
            lines.extend(
                Text::new(
                    key_hint(&self.theme, "tui.select.cancel", "cancel"),
                    1,
                    0,
                    None,
                )
                .render(width),
            );
        }
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines
    }

    /// `handleInput` (bordered-loader.ts:55-59): only the cancellable variant
    /// reacts; Escape/Ctrl+C marks aborted and fires `on_abort`.
    fn handle_input(&mut self, data: &str) {
        if !self.cancellable {
            return;
        }
        let kb = pir_tui::keybindings::get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if read.matches_id(data, "tui.select.cancel") {
            self.aborted = true;
            if let Some(on_abort) = self.on_abort.as_mut() {
                on_abort();
            }
        }
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
    }

    fn noop_render_handle() -> RenderHandle {
        RenderHandle::new(|| {})
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

    #[test]
    fn renders_borders_message_and_cancel_hint_by_default() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let mut component = BorderedLoaderComponent::new(
            noop_render_handle(),
            theme(),
            "Installing extension",
            None,
        );
        assert!(component.is_cancellable(), "cancellable defaults to true");

        let lines: Vec<String> = component.render(50).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines[0].contains('─'), "top border first");
        assert!(lines.last().unwrap().contains('─'), "bottom border last");
        assert!(lines.iter().any(|l| l.contains("Installing extension")));
        assert!(
            lines.iter().any(|l| l.contains("cancel")),
            "cancel hint rendered"
        );
        // A default spinner frame is rendered next to the message.
        let message_line = lines
            .iter()
            .find(|l| l.contains("Installing extension"))
            .unwrap();
        assert!(pir_tui::components::loader::DEFAULT_FRAMES
            .iter()
            .any(|frame| message_line.contains(frame)));
        component.dispose();
    }

    #[test]
    fn set_message_updates_render() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let mut component = BorderedLoaderComponent::new(
            noop_render_handle(),
            theme(),
            "Installing extension",
            None,
        );
        assert!(strip_ansi(&component.render(50).join("\n")).contains("Installing extension"));
        component.set_message("Running");
        assert!(!strip_ansi(&component.render(50).join("\n")).contains("Installing extension"));
        assert!(strip_ansi(&component.render(50).join("\n")).contains("Running"));
        component.dispose();
    }

    #[test]
    fn escape_and_ctrl_c_abort_and_fire_on_abort_every_time() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let aborts = Arc::new(AtomicUsize::new(0));
        let aborts_thread = Arc::clone(&aborts);
        let mut component =
            BorderedLoaderComponent::new(noop_render_handle(), theme(), "Working", None);
        component.on_abort = Some(Box::new(move || {
            aborts_thread.fetch_add(1, Ordering::Relaxed);
        }));

        assert!(!component.aborted());
        component.handle_input("\x1b");
        assert!(component.aborted());
        assert_eq!(aborts.load(Ordering::Relaxed), 1);
        component.handle_input("\x03"); // ctrl+c
        component.handle_input("\x1b");
        assert_eq!(
            aborts.load(Ordering::Relaxed),
            3,
            "fires on every matching keypress"
        );
        component.dispose();
    }

    #[test]
    fn non_cancellable_ignores_escape_and_omits_hint() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let mut component = BorderedLoaderComponent::new(
            noop_render_handle(),
            theme(),
            "Working",
            Some(BorderedLoaderOptions { cancellable: false }),
        );
        assert!(!component.is_cancellable());

        let lines: Vec<String> = component.render(50).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            !lines.iter().any(|l| l.contains("cancel")),
            "no cancel hint"
        );

        component.handle_input("\x1b");
        assert!(!component.aborted(), "non-cancellable must not abort");
        component.dispose();
    }

    #[test]
    fn non_cancel_keys_do_not_abort() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let mut component =
            BorderedLoaderComponent::new(noop_render_handle(), theme(), "Working", None);
        component.handle_input("\r"); // enter
        component.handle_input("\x18"); // ctrl+x
        component.handle_input("a");
        assert!(!component.aborted());
        component.dispose();
    }

    #[test]
    fn dispose_stops_animation_thread() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        // Two frames at a fast interval: the frame must advance while running
        // and freeze after dispose (no thread leak — a leaked thread would
        // keep advancing the frame).
        let mut component =
            BorderedLoaderComponent::new(noop_render_handle(), theme(), "Working", None);
        component
            .loader
            .set_indicator(Some(pir_tui::components::loader::LoaderIndicatorOptions {
                frames: Some(vec!["a".to_string(), "b".to_string()]),
                interval_ms: Some(10),
            }));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut advanced = false;
        while Instant::now() < deadline {
            if strip_ansi(&component.render(30).join("\n")).contains("b Working") {
                advanced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(advanced, "animation should advance frames");

        component.dispose();
        let frozen = component.render(30);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            frozen,
            component.render(30),
            "disposed loader must not animate"
        );
    }
}
