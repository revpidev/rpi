//! Assistant message rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/assistant-message.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816); `markdownTheme` defaults to
//!   the shared interactive theme built by
//!   [`crate::modes::interactive::theme::markdown_theme`].
//! - `updateContent` takes the message by value (upstream reference); the
//!   component stores its own clone.

use std::boxed::Box as StdBox;
use std::sync::Arc;

use pir_ai::types::{AssistantContent, AssistantMessage, StopReason};
use pir_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::tui::{Component, Container};

use crate::core::themes::Theme;

use super::util::{OSC133_ZONE_END, OSC133_ZONE_FINAL, OSC133_ZONE_START};

/// Component that renders a complete assistant message
/// (assistant-message.ts:12-180).
pub struct AssistantMessageComponent {
    content_container: Container,
    hide_thinking_block: bool,
    markdown_theme: Arc<MarkdownTheme>,
    hidden_thinking_label: String,
    output_pad: usize,
    last_message: Option<AssistantMessage>,
    has_tool_calls: bool,
    theme: Arc<Theme>,
}

impl AssistantMessageComponent {
    pub fn new(
        message: Option<AssistantMessage>,
        hide_thinking_block: bool,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
        hidden_thinking_label: impl Into<String>,
        output_pad: usize,
    ) -> Self {
        let mut component = Self {
            content_container: Container::new(),
            hide_thinking_block,
            markdown_theme,
            hidden_thinking_label: hidden_thinking_label.into(),
            output_pad,
            last_message: None,
            has_tool_calls: false,
            theme,
        };
        if let Some(message) = message {
            component.update_content(message);
        }
        component
    }

    /// `setHideThinkingBlock` (assistant-message.ts:51-56).
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        if let Some(message) = self.last_message.clone() {
            self.update_content(message);
        }
    }

    /// `setHiddenThinkingLabel` (assistant-message.ts:58-63).
    pub fn set_hidden_thinking_label(&mut self, label: impl Into<String>) {
        self.hidden_thinking_label = label.into();
        if let Some(message) = self.last_message.clone() {
            self.update_content(message);
        }
    }

    /// `setOutputPad` (assistant-message.ts:65-70).
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        if let Some(message) = self.last_message.clone() {
            self.update_content(message);
        }
    }

    /// `updateContent` (assistant-message.ts:83-179).
    pub fn update_content(&mut self, message: AssistantMessage) {
        self.last_message = Some(message.clone());

        // Clear content container
        self.content_container.clear();

        let has_visible_content = message.content.iter().any(has_visible_content_block);

        if has_visible_content {
            self.content_container
                .add_child(StdBox::new(Spacer::new(1)));
        }

        // Render content in order (assistant-message.ts:98-146).
        let mut i = 0;
        while i < message.content.len() {
            match &message.content[i] {
                // Assistant text messages with no background - trim the text.
                // Set paddingY=0 to avoid extra spacing before tool executions
                // (assistant-message.ts:100-103).
                AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                    let markdown = Markdown::new(
                        text.text.trim(),
                        self.output_pad,
                        0,
                        Arc::clone(&self.markdown_theme),
                        None,
                        None,
                    );
                    self.content_container.add_child(StdBox::new(markdown));
                    i += 1;
                }
                AssistantContent::Thinking(_) => {
                    // Merge consecutive thinking blocks into one run
                    // (assistant-message.ts:104-116).
                    let mut thinking_blocks: Vec<String> = Vec::new();
                    while i < message.content.len() {
                        match &message.content[i] {
                            AssistantContent::Thinking(thinking) => {
                                let trimmed = thinking.thinking.trim();
                                if !trimmed.is_empty() {
                                    thinking_blocks.push(trimmed.to_string());
                                }
                                i += 1;
                            }
                            _ => break,
                        }
                    }

                    if thinking_blocks.is_empty() {
                        continue;
                    }

                    // Add spacing only when another visible assistant content
                    // block follows (assistant-message.ts:122-126).
                    let has_visible_content_after =
                        message.content[i..].iter().any(has_visible_content_block);

                    if self.hide_thinking_block {
                        // Show one static label for each run of thinking
                        // blocks when hidden (assistant-message.ts:128-132).
                        let label = Theme::italic(
                            &self.theme.fg("thinkingText", &self.hidden_thinking_label),
                        );
                        self.content_container.add_child(StdBox::new(Text::new(
                            label,
                            self.output_pad,
                            0,
                            None,
                        )));
                    } else {
                        // Render each run of thinking blocks as one Markdown
                        // section (assistant-message.ts:134-140).
                        let thinking_style = DefaultTextStyle {
                            color: Some(Box::new({
                                let theme = Arc::clone(&self.theme);
                                move |text: &str| theme.fg("thinkingText", text)
                            })),
                            italic: true,
                            ..Default::default()
                        };
                        let markdown = Markdown::new(
                            thinking_blocks.join("\n\n"),
                            self.output_pad,
                            0,
                            Arc::clone(&self.markdown_theme),
                            Some(thinking_style),
                            None,
                        );
                        self.content_container.add_child(StdBox::new(markdown));
                    }
                    if has_visible_content_after {
                        self.content_container
                            .add_child(StdBox::new(Spacer::new(1)));
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }

        // Incomplete/failed handling (assistant-message.ts:148-178).
        let has_tool_calls = message
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        self.has_tool_calls = has_tool_calls;
        if message.stop_reason == StopReason::Length {
            self.content_container
                .add_child(StdBox::new(Spacer::new(1)));
            let text = self.theme.fg(
                "error",
                "Error: Model stopped because it reached the maximum output token limit. The response may be incomplete.",
            );
            self.content_container.add_child(StdBox::new(Text::new(
                text,
                self.output_pad,
                0,
                None,
            )));
        } else if !has_tool_calls {
            match message.stop_reason {
                StopReason::Aborted => {
                    let abort_message = match &message.error_message {
                        Some(msg) if msg != "Request was aborted" => msg.clone(),
                        _ => "Operation aborted".to_string(),
                    };
                    self.content_container
                        .add_child(StdBox::new(Spacer::new(1)));
                    let text = self.theme.fg("error", &abort_message);
                    self.content_container.add_child(StdBox::new(Text::new(
                        text,
                        self.output_pad,
                        0,
                        None,
                    )));
                }
                StopReason::Error => {
                    let error_msg = message
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Unknown error".to_string());
                    self.content_container
                        .add_child(StdBox::new(Spacer::new(1)));
                    let text = self.theme.fg("error", &format!("Error: {error_msg}"));
                    self.content_container.add_child(StdBox::new(Text::new(
                        text,
                        self.output_pad,
                        0,
                        None,
                    )));
                }
                _ => {}
            }
        }
    }
}

/// `hasVisibleContent` predicate (assistant-message.ts:89-91, 124-126).
fn has_visible_content_block(content: &AssistantContent) -> bool {
    match content {
        AssistantContent::Text(text) => !text.text.trim().is_empty(),
        AssistantContent::Thinking(thinking) => !thinking.thinking.trim().is_empty(),
        AssistantContent::ToolCall(_) => false,
    }
}

impl Component for AssistantMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.content_container.render(width);
        // OSC 133 zone markers on the first/last line unless tool calls
        // render separately (assistant-message.ts:72-81).
        if self.has_tool_calls || lines.is_empty() {
            return lines;
        }
        lines[0] = format!("{OSC133_ZONE_START}{}", lines[0]);
        let last = lines.len() - 1;
        lines[last] = format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{}", lines[last]);
        lines
    }

    fn invalidate(&mut self) {
        self.content_container.invalidate();
        if let Some(message) = self.last_message.clone() {
            self.update_content(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::theme::markdown_theme;
    use pir_ai::types::{AssistantRole, TextContent, ThinkingContent, Usage};

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn message(content: Vec<AssistantContent>) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content,
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    fn text(s: &str) -> AssistantContent {
        AssistantContent::Text(TextContent {
            text: s.to_string(),
            text_signature: None,
        })
    }

    fn thinking(s: &str) -> AssistantContent {
        AssistantContent::Thinking(ThinkingContent {
            thinking: s.to_string(),
            thinking_signature: None,
            redacted: None,
        })
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
    fn renders_text_with_osc133_markers() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![text("hello")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let lines = component.render(40);
        assert!(lines[0].starts_with("\u{1b}]133;A\u{7}"));
        assert!(lines[lines.len() - 1].starts_with("\u{1b}]133;B\u{7}\u{1b}]133;C\u{7}"));
        // Text renders with outputPad 1: " hello " (first content line).
        let hello_line = lines
            .iter()
            .find(|l| strip_ansi(l).contains("hello"))
            .expect("hello line");
        assert!(strip_ansi(hello_line).contains(" hello "));

        // Tool calls disable the OSC markers (rendered separately).
        component.update_content(message(vec![
            text("hi"),
            AssistantContent::ToolCall(Default::default()),
        ]));
        let lines = component.render(40);
        assert!(!lines[0].starts_with("\u{1b}]133;A\u{7}"));
    }

    #[test]
    fn hide_thinking_block_shows_italic_label() {
        let component = AssistantMessageComponent::new(
            Some(message(vec![thinking("deep thoughts")])),
            true,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let lines = component.render(40);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("Thinking..."));
        // The label is italic-wrapped.
        assert!(lines.iter().any(|l| l.contains("\u{1b}[3m")));
        assert!(!stripped.contains("deep thoughts"));
    }

    #[test]
    fn thinking_blocks_merge_and_render_markdown() {
        let component = AssistantMessageComponent::new(
            Some(message(vec![
                thinking("first"),
                thinking("second"),
                text("after"),
            ])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let lines = component.render(40);
        let stripped = strip_ansi(&lines.join("\n"));
        // Both thinking blocks rendered, blank-line separated.
        assert!(stripped.contains("first"));
        assert!(stripped.contains("second"));
        assert!(stripped.contains("after"));
        // Italic thinking styling is applied.
        assert!(lines.iter().any(|l| l.contains("\u{1b}[3m")));
    }

    #[test]
    fn stop_reason_length_appends_error_line() {
        let mut m = message(vec![text("partial")]);
        m.stop_reason = StopReason::Length;
        let component = AssistantMessageComponent::new(
            Some(m),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        // Long line wraps at width 40; check a stable fragment.
        assert!(stripped.contains("maximum output token"));
    }

    #[test]
    fn stop_reason_aborted_appends_message() {
        let mut m = message(vec![text("partial")]);
        m.stop_reason = StopReason::Aborted;
        m.error_message = Some("Request was aborted".to_string());
        let component = AssistantMessageComponent::new(
            Some(m),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("Operation aborted"));

        let mut m2 = message(vec![]);
        m2.stop_reason = StopReason::Aborted;
        m2.error_message = Some("User pressed escape".to_string());
        let component2 = AssistantMessageComponent::new(
            Some(m2),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let stripped = strip_ansi(&component2.render(40).join("\n"));
        assert!(stripped.contains("User pressed escape"));
    }

    #[test]
    fn stop_reason_error_appends_error_prefix() {
        let mut m = message(vec![text("partial")]);
        m.stop_reason = StopReason::Error;
        m.error_message = Some("boom".to_string());
        let component = AssistantMessageComponent::new(
            Some(m),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("Error: boom"));
    }

    #[test]
    fn setters_rebuild_content() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![thinking("secret")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
        );
        assert!(strip_ansi(&component.render(40).join("\n")).contains("secret"));
        component.set_hide_thinking_block(true);
        assert!(!strip_ansi(&component.render(40).join("\n")).contains("secret"));
        component.set_hidden_thinking_label("Deliberating");
        assert!(strip_ansi(&component.render(40).join("\n")).contains("Deliberating"));
    }
}
