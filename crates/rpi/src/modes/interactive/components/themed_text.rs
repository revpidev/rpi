//! `ThemedText` (components/themed-text.ts @ bf8e4b953): a `Text` whose
//! content is rebuilt from a theme-aware function on
//! [`Component::invalidate`] (the UI performs that on theme changes), so a
//! theme switch — or the system theme receiving the terminal's colors —
//! cannot leave stale ANSI codes in the startup header, loaded resources, or
//! chat notices.
//!
//! Upstream reads a module-level theme proxy; the port passes the shared
//! active-theme handle ([`ThemeHandle`], the same slot as
//! `InteractiveUi::theme`) explicitly (coding-standards §1.2).

use std::sync::{Arc, Mutex, MutexGuard};

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;

use crate::core::themes::Theme;

/// Shared active-theme slot (matches `InteractiveUi::theme`).
pub type ThemeHandle = Arc<Mutex<Arc<Theme>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Themed text component (upstream `ThemedText`).
pub struct ThemedText {
    theme: ThemeHandle,
    build: Box<dyn Fn(&Theme) -> String + Send + Sync>,
    text: Mutex<Text>,
}

impl ThemedText {
    /// `new ThemedText(build, paddingX, paddingY)`: the initial text is
    /// built eagerly so the first frame needs no invalidation.
    pub fn new(
        theme: ThemeHandle,
        build: impl Fn(&Theme) -> String + Send + Sync + 'static,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        let initial = build(&lock(&theme));
        Self {
            theme,
            build: Box::new(build),
            text: Mutex::new(Text::new(initial, padding_x, padding_y, None)),
        }
    }

    /// Rebuild the text from the current theme (upstream rebuilds lazily in
    /// `render`; the port rebuilds on invalidation, which the UI issues on
    /// every theme change).
    pub fn rebuild(&self) {
        let built = (self.build)(&lock(&self.theme));
        lock(&self.text).set_text(built);
    }
}

impl Component for ThemedText {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.text).render(width)
    }

    fn invalidate(&mut self) {
        self.rebuild();
    }
}
