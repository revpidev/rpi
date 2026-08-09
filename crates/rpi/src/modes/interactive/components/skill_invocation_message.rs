//! Skill invocation message — port of
//! `packages/coding-agent/src/modes/interactive/components/skill-invocation-message.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: same as
//! [`super::compaction_summary_message`] (explicit theme, `Box` composition).

use std::boxed::Box as StdBox;
use std::sync::Arc;

use rpi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use rpi_tui::components::r#box::Box as TuiBox;
use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;

use crate::core::agent_session::ParsedSkillBlock;
use crate::core::themes::Theme;

use super::keybinding_hints::key_text;

/// Component that renders a skill invocation message with collapsed/expanded
/// state (skill-invocation-message.ts:11-55). Uses the same background color
/// as custom messages for visual consistency. Only renders the skill block
/// itself — the user message is rendered separately.
pub struct SkillInvocationMessageComponent {
    expanded: bool,
    skill_block: ParsedSkillBlock,
    markdown_theme: Arc<MarkdownTheme>,
    theme: Arc<Theme>,
    content_box: TuiBox,
}

impl SkillInvocationMessageComponent {
    pub fn new(
        skill_block: ParsedSkillBlock,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
    ) -> Self {
        let mut component = Self {
            expanded: false,
            skill_block,
            markdown_theme,
            theme,
            content_box: TuiBox::new(0, 0, None),
        };
        component.update_display();
        component
    }

    /// `setExpanded` (skill-invocation-message.ts:22-25).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    /// `updateDisplay` (skill-invocation-message.ts:33-54).
    fn update_display(&mut self) {
        let bg = {
            let theme = Arc::clone(&self.theme);
            Box::new(move |t: &str| theme.bg("customMessageBg", t))
        };
        let mut content_box = TuiBox::new(1, 1, Some(bg));

        if self.expanded {
            // Expanded: label + skill name header + full content
            // (skill-invocation-message.ts:37-45).
            let label = self
                .theme
                .fg("customMessageLabel", "\u{1b}[1m[skill]\u{1b}[22m");
            content_box.add_child(StdBox::new(Text::new(label, 0, 0, None)));
            let header = format!("**{}**\n\n", self.skill_block.name);
            let color = {
                let theme = Arc::clone(&self.theme);
                Box::new(move |t: &str| theme.fg("customMessageText", t))
            };
            content_box.add_child(StdBox::new(Markdown::new(
                format!("{header}{}", self.skill_block.content),
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
            // Collapsed: single line - [skill] name (hint to expand)
            // (skill-invocation-message.ts:47-52). Note the label keeps its
            // trailing space inside the styling call.
            let line = format!(
                "{}{}{}",
                self.theme
                    .fg("customMessageLabel", "\u{1b}[1m[skill]\u{1b}[22m "),
                self.theme.fg("customMessageText", &self.skill_block.name),
                self.theme.fg(
                    "dim",
                    &format!(" ({} to expand)", key_text("app.tools.expand"))
                ),
            );
            content_box.add_child(StdBox::new(Text::new(line, 0, 0, None)));
        }

        self.content_box = content_box;
    }
}

impl Component for SkillInvocationMessageComponent {
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

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn block() -> ParsedSkillBlock {
        ParsedSkillBlock {
            name: "plan".into(),
            location: "skills/plan".into(),
            content: "1. Investigate\n2. Write a plan".into(),
            user_message: Some("use the plan skill".into()),
        }
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
    fn collapsed_shows_label_name_and_hint() {
        let component = SkillInvocationMessageComponent::new(
            block(),
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("[skill] plan"));
        assert!(stripped.contains("(ctrl+o to expand)"));
        assert!(!stripped.contains("1. Investigate"));
    }

    #[test]
    fn expanded_shows_header_and_content() {
        let mut component = SkillInvocationMessageComponent::new(
            block(),
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
        );
        component.set_expanded(true);
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("[skill]"));
        assert!(stripped.contains("plan"));
        assert!(stripped.contains("1. Investigate"));
        assert!(stripped.contains("2. Write a plan"));
    }
}
