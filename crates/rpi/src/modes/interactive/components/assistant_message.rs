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

use rpi_ai::types::{AssistantContent, AssistantMessage, StopReason};
use rpi_ext_host::types::MarkdownTransformerFn;
use rpi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, Container};

use crate::core::themes::Theme;

use super::markdown_transform::create_markdown_transform;
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
    /// Fingerprint of everything the `updateContent` rebuild depends on;
    /// `None` until the first rebuild. Equal fingerprints skip the rebuild.
    last_signature: Option<VisibleSignature>,
    has_tool_calls: bool,
    theme: Arc<Theme>,
    /// Extension-registered Markdown transformers (assistant-message.ts:20).
    markdown_transformers: Vec<MarkdownTransformerFn>,
    /// `isStreaming` (assistant-message.ts:23): set by `updateContent`'s
    /// argument and captured into the transform context.
    is_streaming: bool,
}

/// Cheap fingerprint of the rebuild inputs: streaming flag, thinking-block
/// settings, pad, stop reason / error text, tool-call presence, and a
/// (length, hash) pair per visible text/thinking block. Everything the
/// rebuilt children and the render-time OSC133 markers depend on.
type VisibleSignature = (
    bool,
    bool,
    String,
    usize,
    StopReason,
    Option<String>,
    bool,
    Vec<(u64, u64)>,
);

/// (len, hash) fingerprint of a visible block's content.
fn block_fingerprint(text: &str) -> (u64, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    (text.len() as u64, hasher.finish())
}

#[cfg(test)]
thread_local! {
    /// Counts container rebuilds inside `update_content` (perf tests).
    static REBUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl AssistantMessageComponent {
    pub fn new(
        message: Option<AssistantMessage>,
        hide_thinking_block: bool,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
        hidden_thinking_label: impl Into<String>,
        output_pad: usize,
        markdown_transformers: Vec<MarkdownTransformerFn>,
    ) -> Self {
        let mut component = Self {
            content_container: Container::new(),
            hide_thinking_block,
            markdown_theme,
            hidden_thinking_label: hidden_thinking_label.into(),
            output_pad,
            last_message: None,
            last_signature: None,
            has_tool_calls: false,
            theme,
            markdown_transformers,
            is_streaming: false,
        };
        if let Some(message) = message {
            component.update_content(message, false);
        }
        component
    }

    /// `setHideThinkingBlock` (assistant-message.ts:51-56).
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        if let Some(message) = self.last_message.clone() {
            self.update_content(message, self.is_streaming);
        }
    }

    /// `setHiddenThinkingLabel` (assistant-message.ts:58-63).
    pub fn set_hidden_thinking_label(&mut self, label: impl Into<String>) {
        self.hidden_thinking_label = label.into();
        if let Some(message) = self.last_message.clone() {
            self.update_content(message, self.is_streaming);
        }
    }

    /// `setOutputPad` (assistant-message.ts:65-70).
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        if let Some(message) = self.last_message.clone() {
            self.update_content(message, self.is_streaming);
        }
    }

    /// `updateContent` (assistant-message.ts:89-179). `is_streaming` is the
    /// upstream `isStreaming` argument (`updateContent(message, isStreaming =
    /// this.isStreaming)` — the setters and `invalidate` keep the stored
    /// value).
    ///
    /// Perf deviation: upstream rebuilds the content container on every
    /// call. During tool-call streaming this runs once per delta; when the
    /// visible fingerprint is unchanged (a write/edit streaming its args —
    /// no text/thinking/stop changes), the rebuild is skipped and only the
    /// stored message is refreshed.
    pub fn update_content(&mut self, message: AssistantMessage, is_streaming: bool) {
        self.is_streaming = is_streaming;

        let signature = (
            is_streaming,
            self.hide_thinking_block,
            self.hidden_thinking_label.clone(),
            self.output_pad,
            message.stop_reason,
            message.error_message.clone(),
            message
                .content
                .iter()
                .any(|c| matches!(c, AssistantContent::ToolCall(_))),
            message
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(text) => Some(block_fingerprint(&text.text)),
                    AssistantContent::Thinking(thinking) => {
                        Some(block_fingerprint(&thinking.thinking))
                    }
                    AssistantContent::ToolCall(_) => None,
                })
                .collect::<Vec<_>>(),
        );
        if self.last_signature.as_ref() == Some(&signature) {
            self.last_message = Some(message);
            return;
        }
        self.last_signature = Some(signature);
        self.last_message = Some(message);
        let message = self.last_message.as_ref().expect("stored above");
        #[cfg(test)]
        REBUILD_COUNT.with(|count| count.set(count.get() + 1));

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
                        self.markdown_options("assistant"),
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
                            self.markdown_options("assistant-thinking"),
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
        // Length stops: neutral truncation wording (assistant-message.ts:180
        // @ 32850ef7c).
        if message.stop_reason == StopReason::Length {
            self.content_container
                .add_child(StdBox::new(Spacer::new(1)));
            let text = self
                .theme
                .fg("error", "Response was truncated before completion.");
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

    /// Markdown options carrying the width-aware transform chain for the
    /// block's message type (assistant-message.ts:110-113, 156-162); `None`
    /// when no transformer is registered (the Markdown renders unchanged).
    fn markdown_options(&self, message_type: &str) -> Option<MarkdownOptions> {
        create_markdown_transform(
            message_type,
            self.is_streaming,
            self.markdown_transformers.clone(),
        )
        .map(|transform| MarkdownOptions {
            transform: Some(transform),
            ..Default::default()
        })
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
            self.update_content(message, self.is_streaming);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::theme::markdown_theme;
    use rpi_ai::types::{AssistantRole, TextContent, ThinkingContent, Usage};

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
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
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

    /// Perf regression (300+ line streaming writes): `MessageUpdate` fires
    /// once per tool-args delta; updates that only change tool-call blocks
    /// (no text/thinking/stop/error changes) must skip the container
    /// rebuild entirely.
    #[test]
    fn tool_call_only_updates_skip_the_rebuild() {
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let tool_call = |args_len: usize| {
            AssistantContent::ToolCall(rpi_ai::types::ToolCall {
                id: "t1".into(),
                name: "write".into(),
                arguments: serde_json::Map::from_iter([(
                    "content".to_owned(),
                    serde_json::Value::String("x".repeat(args_len)),
                )]),
                thought_signature: None,
                namespace: None,
            })
        };

        REBUILD_COUNT.with(|count| count.set(0));
        // Visible text first — rebuilds.
        component.update_content(
            message(vec![text("Writing the file:"), tool_call(10)]),
            true,
        );
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 1);

        // 300 tool-args-only deltas: no further rebuilds.
        for len in 11..=310 {
            component.update_content(
                message(vec![text("Writing the file:"), tool_call(len)]),
                true,
            );
        }
        assert_eq!(
            REBUILD_COUNT.with(|c| c.get()),
            1,
            "tool-call-only deltas must not rebuild the container"
        );

        // The rendered text survives the skipped rebuilds.
        let lines = component.render(60);
        assert!(lines.iter().any(|l| l.contains("Writing the file:")));

        // isStreaming transition (message_end) rebuilds exactly once more.
        component.update_content(
            message(vec![text("Writing the file:"), tool_call(310)]),
            false,
        );
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 2);

        // A text delta rebuilds.
        component.update_content(
            message(vec![text("Writing the file: done"), tool_call(310)]),
            false,
        );
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 3);
    }

    /// `set_output_pad` and the thinking setters must not be swallowed by
    /// the signature skip: they change the rebuild inputs and must rebuild.
    #[test]
    fn setters_bypass_the_signature_skip() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![text("hello")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        REBUILD_COUNT.with(|count| count.set(0));
        component.set_output_pad(2);
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 1, "pad change rebuilds");
        component.set_hide_thinking_block(true);
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 2, "hide toggle rebuilds");
        component.set_hidden_thinking_label("Thoughts");
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 3, "label change rebuilds");
        // A same-value setter is a no-op (the fingerprint is unchanged and
        // the rendering would be identical anyway).
        component.set_output_pad(2);
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 3);
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
            Vec::new(),
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
        component.update_content(
            message(vec![
                text("hi"),
                AssistantContent::ToolCall(Default::default()),
            ]),
            false,
        );
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
        );
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("Response was truncated before completion."));
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
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
            Vec::new(),
        );
        assert!(strip_ansi(&component.render(40).join("\n")).contains("secret"));
        component.set_hide_thinking_block(true);
        assert!(!strip_ansi(&component.render(40).join("\n")).contains("secret"));
        component.set_hidden_thinking_label("Deliberating");
        assert!(strip_ansi(&component.render(40).join("\n")).contains("Deliberating"));
    }

    #[test]
    fn applies_markdown_transform_chain_with_assistant_context() {
        // T29: the extension transformer chain applies in order; the context
        // carries messageType "assistant" and the (non-streaming) flag of
        // the constructor path (assistant-message.ts:110-113).
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_a = Arc::clone(&seen);
        let seen_b = Arc::clone(&seen);
        let component = AssistantMessageComponent::new(
            Some(message(vec![text("hello")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            vec![
                Arc::new(move |md, ctx| {
                    seen_a
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(("a".to_owned(), ctx.clone()));
                    format!("[a]{md}")
                }),
                Arc::new(move |md, ctx| {
                    seen_b
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(("b".to_owned(), ctx.clone()));
                    format!("{md}[b]")
                }),
            ],
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("[a]hello[b]"), "stripped: {stripped}");

        let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[1].0, "b");
        for (_, context) in &calls {
            assert_eq!(context.message_type, "assistant");
            assert!(!context.is_streaming);
            // width - padding_x * 2 = 40 - 2.
            assert_eq!(context.available_width, 38);
        }
    }

    #[test]
    fn thinking_transform_context_uses_streaming_flag() {
        // assistant-message.ts:156-162: thinking blocks transform with
        // messageType "assistant-thinking"; a streaming update marks
        // isStreaming true.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_inner = Arc::clone(&seen);
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            vec![Arc::new(move |md, ctx| {
                seen_inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ctx.clone());
                md
            })],
        );
        component.update_content(message(vec![thinking("deep")]), true);
        let _ = component.render(50);
        let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message_type, "assistant-thinking");
        assert!(calls[0].is_streaming);
        // width - padding_x * 2 = 50 - 2.
        assert_eq!(calls[0].available_width, 48);
    }
}
