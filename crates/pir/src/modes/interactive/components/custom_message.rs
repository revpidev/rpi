//! Custom message rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/custom-message.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816); `markdownTheme` is explicit.
//! - [`MessageRenderer`] returns `Option<Box<dyn Component>>` (upstream
//!   `Component | undefined`); a `None` return or a renderer that panics
//!   falls through to the default box rendering, matching upstream's
//!   try/catch (custom-message.ts:70-85). Renderer panics are NOT caught —
//!   Rust has no safe cross-component catch; the extension host (T15) will
//!   isolate renderer execution.
//! - Upstream keeps `this.box`/`this.customComponent` as persistent children
//!   and removes them by identity in `rebuild`; the port clears the
//!   container and rebuilds (identical rendered output).

use std::boxed::Box as StdBox;
use std::sync::Arc;

use pir_agent::messages::CustomMessage;
use pir_ai::types::UserContent;
use pir_ai::types::UserContentBlock;
use pir_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use pir_tui::components::r#box::Box as TuiBox;
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::tui::{Component, Container};

use crate::core::themes::Theme;

/// `MessageRenderer` (extensions/types.ts:1140-1144): a custom renderer for
/// [`CustomMessage`]s, invoked before the default box rendering. The renderer
/// owns all styling of the returned component.
pub type MessageRenderer = StdBox<
    dyn Fn(&CustomMessage, MessageRenderOptions, &Theme) -> Option<StdBox<dyn Component>>
        + Send
        + Sync,
>;

/// `MessageRenderOptions` (extensions/types.ts:1130-1134).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRenderOptions {
    pub expanded: bool,
    pub output_pad: usize,
}

/// Component that renders a custom message entry from extensions
/// (custom-message.ts:12-113). Uses distinct styling to differentiate from
/// user messages.
pub struct CustomMessageComponent {
    message: CustomMessage,
    custom_renderer: Option<MessageRenderer>,
    markdown_theme: Arc<MarkdownTheme>,
    theme: Arc<Theme>,
    expanded: bool,
    output_pad: usize,
    // Children: [Spacer(1), content (custom component or box)].
    container: Container,
}

impl CustomMessageComponent {
    pub fn new(
        message: CustomMessage,
        custom_renderer: Option<MessageRenderer>,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
        output_pad: usize,
    ) -> Self {
        let mut component = Self {
            message,
            custom_renderer,
            markdown_theme,
            theme,
            expanded: false,
            output_pad,
            container: Container::new(),
        };
        component.container.add_child(StdBox::new(Spacer::new(1)));
        component.rebuild();
        component
    }

    /// `setExpanded` (custom-message.ts:41-46).
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    /// `setOutputPad` (custom-message.ts:48-53).
    pub fn set_output_pad(&mut self, output_pad: usize) {
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            self.rebuild();
        }
    }

    /// `rebuild` (custom-message.ts:60-112): try the custom renderer first
    /// (it handles its own styling); otherwise render the default box.
    fn rebuild(&mut self) {
        self.container.clear();
        self.container.add_child(StdBox::new(Spacer::new(1)));

        // Try custom renderer first (custom-message.ts:69-85).
        if let Some(renderer) = &self.custom_renderer {
            if let Some(component) = renderer(
                &self.message,
                MessageRenderOptions {
                    expanded: self.expanded,
                    output_pad: self.output_pad,
                },
                &self.theme,
            ) {
                self.container.add_child(component);
                return;
            }
        }

        // Default rendering uses our box (custom-message.ts:88-111).
        let bg = {
            let theme = Arc::clone(&self.theme);
            Box::new(move |t: &str| theme.bg("customMessageBg", t))
        };
        let mut box_component = TuiBox::new(1, 1, Some(bg));

        // Default rendering: label + content.
        let label = self.theme.fg(
            "customMessageLabel",
            &format!("\u{1b}[1m[{}]\u{1b}[22m", self.message.custom_type),
        );
        box_component.add_child(StdBox::new(Text::new(label, 0, 0, None)));
        box_component.add_child(StdBox::new(Spacer::new(1)));

        // Extract text content (custom-message.ts:96-105).
        let text = match &self.message.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    UserContentBlock::Text(text) => Some(text.text.clone()),
                    UserContentBlock::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };

        let color = {
            let theme = Arc::clone(&self.theme);
            Box::new(move |t: &str| theme.fg("customMessageText", t))
        };
        box_component.add_child(StdBox::new(Markdown::new(
            text,
            0,
            0,
            Arc::clone(&self.markdown_theme),
            Some(DefaultTextStyle {
                color: Some(color),
                ..Default::default()
            }),
            None,
        )));
        self.container.add_child(StdBox::new(box_component));
    }
}

impl Component for CustomMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::theme::markdown_theme;
    use pir_agent::messages::CustomRole;
    use pir_tui::components::text::Text as TuiText;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn message(content: UserContent) -> CustomMessage {
        CustomMessage {
            role: CustomRole::Custom,
            custom_type: "test".into(),
            content,
            display: true,
            details: None,
            timestamp: 0,
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
    fn default_rendering_shows_label_and_content() {
        let component = CustomMessageComponent::new(
            message(UserContent::Text("hi there".into())),
            None,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            1,
        );
        let lines = component.render(40);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("[test]"));
        assert!(stripped.contains("hi there"));
        // Box background is applied.
        assert!(lines.iter().any(|l| l.contains("\u{1b}[48;")));
    }

    #[test]
    fn block_content_extracts_text_only() {
        use pir_ai::types::{ImageContent, TextContent};
        let content = UserContent::Blocks(vec![
            UserContentBlock::Text(TextContent {
                text: "line one".into(),
                text_signature: None,
            }),
            UserContentBlock::Image(ImageContent {
                data: "data".into(),
                mime_type: "image/png".into(),
            }),
            UserContentBlock::Text(TextContent {
                text: "line two".into(),
                text_signature: None,
            }),
        ]);
        let component = CustomMessageComponent::new(
            message(content),
            None,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            1,
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("line one"));
        assert!(stripped.contains("line two"));
    }

    #[test]
    fn custom_renderer_is_preferred_and_gets_options() {
        // Upstream test custom-message.test.ts: "provides output padding to
        // custom renderers and updates it".
        let options_seen: Arc<std::sync::Mutex<Vec<MessageRenderOptions>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let options_thread = Arc::clone(&options_seen);
        let renderer: MessageRenderer = Box::new(move |_message, options, _theme| {
            options_thread.lock().unwrap().push(options);
            Some(StdBox::new(TuiText::new(
                "custom",
                options.output_pad,
                0,
                None,
            )))
        });
        let mut component = CustomMessageComponent::new(
            message(UserContent::Text("custom".into())),
            Some(renderer),
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            1,
        );

        assert_eq!(
            *options_seen.lock().unwrap(),
            vec![MessageRenderOptions {
                expanded: false,
                output_pad: 1
            }]
        );
        assert!(component
            .render(40)
            .iter()
            .map(|l| strip_ansi(l))
            .any(|line| line.starts_with(" custom")));

        component.set_output_pad(0);
        assert_eq!(
            *options_seen.lock().unwrap(),
            vec![
                MessageRenderOptions {
                    expanded: false,
                    output_pad: 1
                },
                MessageRenderOptions {
                    expanded: false,
                    output_pad: 0
                }
            ]
        );
        assert!(component
            .render(40)
            .iter()
            .map(|l| strip_ansi(l))
            .any(|line| line.starts_with("custom")));
    }

    #[test]
    fn renderer_returning_none_falls_back_to_default() {
        let renderer: MessageRenderer = Box::new(|_message, _options, _theme| None);
        let component = CustomMessageComponent::new(
            message(UserContent::Text("fallback".into())),
            Some(renderer),
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            1,
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("[test]"));
        assert!(stripped.contains("fallback"));
    }
}
