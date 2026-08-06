//! User message rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/user-message.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816); `markdownTheme` is an
//!   explicit `Arc<MarkdownTheme>`.
//! - Upstream keeps `this.box` as a persistent child and re-adds it in
//!   `rebuild`; the port recreates the box on each rebuild (identical
//!   rendered output).

use std::boxed::Box as StdBox;
use std::sync::Arc;

use pir_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use pir_tui::components::r#box::Box as TuiBox;
use pir_tui::tui::Component;

use crate::core::themes::Theme;

use super::util::{OSC133_ZONE_END, OSC133_ZONE_FINAL, OSC133_ZONE_START};

/// Component that renders a user message (user-message.ts:11-57).
pub struct UserMessageComponent {
    text: String,
    markdown_theme: Arc<MarkdownTheme>,
    output_pad: usize,
    theme: Arc<Theme>,
    content_box: TuiBox,
}

impl UserMessageComponent {
    pub fn new(
        text: impl Into<String>,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
        output_pad: usize,
    ) -> Self {
        let mut component = Self {
            text: text.into(),
            markdown_theme,
            output_pad,
            theme,
            content_box: TuiBox::new(0, 0, None),
        };
        component.rebuild();
        component
    }

    /// `setOutputPad` (user-message.ts:24-27).
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        self.rebuild();
    }

    /// `rebuild` (user-message.ts:29-45).
    fn rebuild(&mut self) {
        let bg = {
            let theme = Arc::clone(&self.theme);
            Box::new(move |content: &str| theme.bg("userMessageBg", content))
        };
        let mut content_box = TuiBox::new(self.output_pad, 1, Some(bg));
        let color = {
            let theme = Arc::clone(&self.theme);
            Box::new(move |content: &str| theme.fg("userMessageText", content))
        };
        content_box.add_child(StdBox::new(Markdown::new(
            self.text.clone(),
            0,
            0,
            Arc::clone(&self.markdown_theme),
            Some(DefaultTextStyle {
                color: Some(color),
                ..Default::default()
            }),
            Some(MarkdownOptions {
                preserve_ordered_list_markers: true,
                preserve_backslash_escapes: true,
            }),
        )));
        self.content_box = content_box;
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.content_box.render(width);
        if lines.is_empty() {
            return lines;
        }
        // OSC 133 zone markers on the first/last line
        // (user-message.ts:47-55).
        lines[0] = format!("{OSC133_ZONE_START}{}", lines[0]);
        let last = lines.len() - 1;
        lines[last] = format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{}", lines[last]);
        lines
    }

    fn invalidate(&mut self) {
        self.content_box.invalidate();
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::theme::markdown_theme;
    use pir_tui::utils::visible_width;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
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
    fn renders_boxed_message_with_osc133_markers() {
        // Upstream test user-message.test.ts: "keeps user message height
        // stable while moving closing OSC markers off line end".
        let component = UserMessageComponent::new(
            "hello",
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            1,
        );
        let lines = component.render(20);

        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\u{1b}]133;A\u{7}"));
        assert!(lines[0].ends_with("\u{1b}[49m"));
        assert!(!lines[0].contains("\u{1b}]133;B\u{7}"));
        assert!(strip_ansi(&lines[1]).contains("hello"));
        assert!(lines[2].starts_with("\u{1b}]133;B\u{7}\u{1b}]133;C\u{7}"));
        assert!(lines[2].ends_with("\u{1b}[49m"));
        for line in &lines {
            assert_eq!(visible_width(line), 20);
        }
    }

    #[test]
    fn set_output_pad_rebuilds() {
        let mut component = UserMessageComponent::new(
            "hello",
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            1,
        );
        assert!(strip_ansi(&component.render(20)[1]).starts_with(" hello "));
        component.set_output_pad(0);
        assert!(strip_ansi(&component.render(20)[1]).starts_with("hello"));
    }
}
