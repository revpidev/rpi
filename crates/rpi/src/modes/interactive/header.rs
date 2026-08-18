//! Startup header — `ExpandableText` (interactive-mode.ts:168-190) and the
//! built-in startup header content (interactive-mode.ts:731-790) @ pi 0.82.1
//! (2efa728).
//!
//! Intentional differences:
//! - The upstream `ExpandableText` extends `Text` and calls
//!   `super.setText`; the port wraps a rpi-tui `Text` and delegates.
//! - Header text builders take an explicit `&Theme` (explicit-injection
//!   convention, coding-standards §1.2); upstream reads the process-global
//!   `theme`.

use std::sync::Arc;

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;

use crate::config::APP_NAME;
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text, raw_key_hint};

/// `ExpandableText` (interactive-mode.ts:168-190): a `Text` whose content is
/// one of two closures selected by the expansion state.
pub struct ExpandableText {
    text: Text,
    get_collapsed_text: Box<dyn Fn() -> String + Send>,
    get_expanded_text: Box<dyn Fn() -> String + Send>,
    expanded: bool,
}

impl ExpandableText {
    pub fn new(
        get_collapsed_text: Box<dyn Fn() -> String + Send>,
        get_expanded_text: Box<dyn Fn() -> String + Send>,
        expanded: bool,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        let text = if expanded {
            get_expanded_text()
        } else {
            get_collapsed_text()
        };
        Self {
            text: Text::new(text, padding_x, padding_y, None),
            get_collapsed_text,
            get_expanded_text,
            expanded,
        }
    }

    /// `setExpanded` (interactive-mode.ts:187-189).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        let text = if expanded {
            (self.get_expanded_text)()
        } else {
            (self.get_collapsed_text)()
        };
        self.text.set_text(text);
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }
}

impl Component for ExpandableText {
    fn render(&self, width: usize) -> Vec<String> {
        self.text.render(width)
    }

    fn invalidate(&mut self) {}

    fn set_expanded(&mut self, expanded: bool) {
        // `setToolsExpanded` walk over the loaded-resources container
        // (upstream `isExpandable` duck-typing); inherent methods win on
        // concrete receivers, so no recursion.
        self.set_expanded(expanded);
    }
}

/// The startup logo line (`logo`, interactive-mode.ts:733).
pub fn startup_logo(theme: &Theme, version: &str) -> String {
    let logo = theme.fg("accent", &Theme::bold(APP_NAME));
    format!("{logo}{}", theme.fg("dim", &format!(" v{version}")))
}

/// Build the two startup-instruction bodies (interactive-mode.ts:735-773):
/// `(expanded, compact)`.
pub fn startup_instructions(theme: &Theme) -> (String, String) {
    let hint = |keybinding: &str, description: &str| key_hint(theme, keybinding, description);

    let expanded_instructions = [
        hint("app.interrupt", "to interrupt"),
        hint("app.clear", "to clear"),
        raw_key_hint(
            theme,
            &format!("{} twice", key_text("app.clear")),
            "to exit",
        ),
        hint("app.exit", "to exit (empty)"),
        hint("app.suspend", "to suspend"),
        key_hint(theme, "tui.editor.deleteToLineEnd", "to delete to end"),
        hint("app.thinking.cycle", "to cycle thinking level"),
        raw_key_hint(
            theme,
            &format!(
                "{}/{}",
                key_text("app.model.cycleForward"),
                key_text("app.model.cycleBackward")
            ),
            "to cycle models",
        ),
        hint("app.model.select", "to select model"),
        hint("app.tools.expand", "to expand tools"),
        hint("app.thinking.toggle", "to expand thinking"),
        hint("app.editor.external", "for external editor"),
        raw_key_hint(theme, "/", "for commands"),
        raw_key_hint(theme, "!", "to run bash"),
        raw_key_hint(theme, "!!", "to run bash (no context)"),
        hint("app.message.followUp", "to queue follow-up"),
        hint("app.message.dequeue", "to edit all queued messages"),
        hint(
            "app.clipboard.pasteImage",
            "to paste image (with text fallback)",
        ),
        raw_key_hint(theme, "drop files", "to attach"),
    ]
    .join("\n");

    let compact_instructions = [
        hint("app.interrupt", "interrupt"),
        raw_key_hint(
            theme,
            &format!("{}/{}", key_text("app.clear"), key_text("app.exit")),
            "clear/exit",
        ),
        raw_key_hint(theme, "/", "commands"),
        raw_key_hint(theme, "!", "bash"),
        hint("app.tools.expand", "more"),
    ]
    .join(&theme.fg("muted", " · "));

    (expanded_instructions, compact_instructions)
}

/// `onboarding` text (interactive-mode.ts:770-773). rpi difference: upstream
/// promises "look up its docs" (bundled docs fed into the system prompt);
/// rpi ships no bundled docs (`doc_paths` is `None`, system-prompt.rs), so
/// the line states the actual capability and points at the site docs.
pub fn startup_onboarding(theme: &Theme) -> String {
    theme.fg(
        "dim",
        "rpi can explain its own features. Ask how to use or extend it — docs at revpi.dev/docs.",
    )
}

/// Build the built-in header component (interactive-mode.ts:732-790).
///
/// Returns `(tree entry, shared handle)` — the mode keeps the shared handle
/// for the tools-expansion linkage and the tree owns the entry wrapper. When
/// `quiet_startup` is set the header is an empty `Text` (upstream assigns
/// `new Text("", 0, 0)` and the expansion linkage skips it via the
/// `isExpandable` check).
pub fn build_builtin_header(
    theme: Arc<Theme>,
    version: &str,
    expanded: bool,
    quiet_startup: bool,
) -> (
    Box<dyn Component>,
    Option<Arc<std::sync::Mutex<ExpandableText>>>,
) {
    if quiet_startup {
        return (Box::new(Text::new("", 0, 0, None)), None);
    }
    let logo = startup_logo(&theme, version);
    let (expanded_instructions, compact_instructions) = startup_instructions(&theme);
    let onboarding = startup_onboarding(&theme);
    let compact_onboarding = theme.fg(
        "dim",
        &format!(
            "Press {} to show full startup help and loaded resources.",
            key_text("app.tools.expand")
        ),
    );
    let collapsed = format!("{logo}\n{compact_instructions}\n{compact_onboarding}\n\n{onboarding}");
    let expanded_text = format!("{logo}\n{expanded_instructions}\n\n{onboarding}");
    let expandable = Arc::new(std::sync::Mutex::new(ExpandableText::new(
        Box::new(move || collapsed.clone()),
        Box::new(move || expanded_text.clone()),
        expanded,
        1,
        0,
    )));
    (
        Box::new(ExpandableTextRegion(Arc::clone(&expandable))),
        Some(expandable),
    )
}

/// Tree entry rendering through the shared [`ExpandableText`] handle (the
/// mode keeps the concrete handle for the tools-expansion linkage).
pub struct ExpandableTextRegion(Arc<std::sync::Mutex<ExpandableText>>);

impl Component for ExpandableTextRegion {
    fn render(&self, width: usize) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .render(width)
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme must load"))
    }

    #[test]
    fn expandable_text_switches_between_two_states() {
        let mut component = ExpandableText::new(
            Box::new(|| "collapsed".to_string()),
            Box::new(|| "expanded".to_string()),
            false,
            0,
            0,
        );
        assert_eq!(component.render(80)[0].trim_end(), "collapsed");
        assert!(!component.is_expanded());
        component.set_expanded(true);
        assert_eq!(component.render(80)[0].trim_end(), "expanded");
        assert!(component.is_expanded());
        component.set_expanded(false);
        assert_eq!(component.render(80)[0].trim_end(), "collapsed");
    }

    #[test]
    fn header_builds_compact_and_expanded_instruction_sets() {
        let theme = theme();
        let (expanded, compact) = startup_instructions(&theme);
        // Compact: 5 hints joined by the muted separator.
        assert_eq!(
            compact.matches(theme.fg("muted", " · ").as_str()).count(),
            4
        );
        assert!(compact.contains("interrupt"));
        assert!(compact.contains("clear/exit"));
        assert!(compact.contains("commands"));
        assert!(compact.contains("bash"));
        assert!(compact.contains("more"));
        // Expanded: 19 instruction lines, one per hint.
        let lines: Vec<&str> = expanded.lines().collect();
        assert_eq!(lines.len(), 19);
        assert!(lines.iter().any(|l| l.contains("to interrupt")));
        assert!(lines.iter().any(|l| l.contains("to exit (empty)")));
    }

    #[test]
    fn quiet_startup_yields_empty_header() {
        let (component, expandable) = build_builtin_header(theme(), "0.1.0", false, true);
        assert!(expandable.is_none());
        assert!(
            component.render(80).is_empty(),
            "quiet header renders nothing"
        );
    }

    #[test]
    fn non_quiet_header_is_expandable_and_starts_collapsed() {
        let (component, expandable) = build_builtin_header(theme(), "0.1.0", false, false);
        let expandable = expandable.expect("expandable header");
        let rendered = component.render(80);
        assert!(rendered.len() > 1);
        assert!(rendered[0].contains("rpi"));
        let guard = expandable.lock().unwrap();
        assert!(!guard.is_expanded());
        assert!(
            rendered.iter().any(|l| l.contains("Press")),
            "compact onboarding: {rendered:?}"
        );
    }
}
