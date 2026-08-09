//! Branch summary message — port of
//! `packages/coding-agent/src/modes/interactive/components/branch-summary-message.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: same as
//! [`super::compaction_summary_message`] (explicit theme, `Box` composition).

use std::boxed::Box as StdBox;
use std::sync::Arc;

use pir_agent::messages::BranchSummaryMessage;
use pir_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use pir_tui::components::r#box::Box as TuiBox;
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::tui::Component;

use crate::core::themes::Theme;

use super::keybinding_hints::key_text;

/// Component that renders a branch summary message with collapsed/expanded
/// state (branch-summary-message.ts:10-58). Uses the same background color
/// as custom messages for visual consistency.
pub struct BranchSummaryMessageComponent {
    expanded: bool,
    message: BranchSummaryMessage,
    markdown_theme: Arc<MarkdownTheme>,
    theme: Arc<Theme>,
    content_box: TuiBox,
}

impl BranchSummaryMessageComponent {
    pub fn new(
        message: BranchSummaryMessage,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
    ) -> Self {
        let mut component = Self {
            expanded: false,
            message,
            markdown_theme,
            theme,
            content_box: TuiBox::new(0, 0, None),
        };
        component.update_display();
        component
    }

    /// `setExpanded` (branch-summary-message.ts:22-25).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    /// `updateDisplay` (branch-summary-message.ts:32-56).
    fn update_display(&mut self) {
        let bg = {
            let theme = Arc::clone(&self.theme);
            Box::new(move |t: &str| theme.bg("customMessageBg", t))
        };
        let mut content_box = TuiBox::new(1, 1, Some(bg));

        let label = self
            .theme
            .fg("customMessageLabel", "\u{1b}[1m[branch]\u{1b}[22m");
        content_box.add_child(StdBox::new(Text::new(label, 0, 0, None)));
        content_box.add_child(StdBox::new(Spacer::new(1)));

        if self.expanded {
            let header = "**Branch Summary**\n\n";
            let color = {
                let theme = Arc::clone(&self.theme);
                Box::new(move |t: &str| theme.fg("customMessageText", t))
            };
            content_box.add_child(StdBox::new(Markdown::new(
                format!("{header}{}", self.message.summary),
                0,
                0,
                Arc::clone(&self.markdown_theme),
                Some(DefaultTextStyle {
                    color: Some(color),
                    ..Default::default()
                }),
                None,
            )));
        } else {
            let text = format!(
                "{}{}{}",
                self.theme.fg("customMessageText", "Branch summary ("),
                self.theme.fg("dim", &key_text("app.tools.expand")),
                self.theme.fg("customMessageText", " to expand)"),
            );
            content_box.add_child(StdBox::new(Text::new(text, 0, 0, None)));
        }

        self.content_box = content_box;
    }
}

impl Component for BranchSummaryMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.content_box.render(width)
    }

    fn invalidate(&mut self) {
        self.content_box.invalidate();
        self.update_display();
    }

    fn set_expanded(&mut self, expanded: bool) {
        // `setToolsExpanded` chat walk (upstream `isExpandable` duck-typing);
        // inherent methods win on concrete receivers, so no recursion.
        self.set_expanded(expanded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::theme::markdown_theme;
    use pir_agent::messages::BranchSummaryRole;

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
    fn collapsed_shows_label_and_hint() {
        let component = BranchSummaryMessageComponent::new(
            BranchSummaryMessage {
                role: BranchSummaryRole::BranchSummary,
                summary: "We refactored the parser.".into(),
                from_id: "e1".into(),
                timestamp: 0,
            },
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("[branch]"));
        assert!(stripped.contains("Branch summary ("));
        assert!(stripped.contains("ctrl+o"));
        assert!(stripped.contains(" to expand)"));
        assert!(!stripped.contains("refactored"));
    }

    #[test]
    fn expanded_shows_summary_markdown() {
        let mut component = BranchSummaryMessageComponent::new(
            BranchSummaryMessage {
                role: BranchSummaryRole::BranchSummary,
                summary: "We refactored the parser.".into(),
                from_id: "e1".into(),
                timestamp: 0,
            },
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
        );
        component.set_expanded(true);
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("Branch Summary"));
        assert!(stripped.contains("We refactored the parser."));
    }
}
