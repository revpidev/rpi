//! Bash execution rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/bash-execution.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - The `Loader` is shared through `Arc<Mutex<Loader>>`: the same loader
//!   instance must live both as a child of the content container (when
//!   running) and as a field that `setComplete` stops. Upstream holds one JS
//!   object reference; the port wraps it in a [`LoaderRef`] component each
//!   time it is (re)attached. `Loader::render` takes `&self`, so the mutex
//!   is only contended on `stop`/`set_message` from the mode thread.
//! - The inline caching render closure (bash-execution.ts:153-166) becomes a
//!   named [`CachedVisualTruncation`] component.
//! - The constructor header is colored with `colorKey` (`dim` when excluded
//!   from context) while `updateDisplay` re-adds it with `"bashMode"`
//!   (bash-execution.ts:51, 138) — this upstream quirk is ported as-is.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use rpi_tui::components::loader::Loader;
use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, Container, RenderHandle};

use crate::core::themes::Theme;
use crate::tools::sanitize::strip_ansi;
use crate::tools::truncate::{
    truncate_tail, TruncateOptions, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{key_hint, key_text};
use super::visual_truncate::truncate_to_visual_lines;

/// Preview line limit when not expanded (matches tool execution behavior,
/// bash-execution.ts:19).
const PREVIEW_LINES: usize = 20;

/// `status` (bash-execution.ts:24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashStatus {
    Running,
    Complete,
    Cancelled,
    Error,
}

/// Shared loader reference: re-attachable child component wrapping the
/// [`Loader`] owned by [`BashExecutionComponent`].
struct LoaderRef(Arc<Mutex<Loader>>);

impl Component for LoaderRef {
    fn render(&self, width: usize) -> Vec<String> {
        let Ok(loader) = self.0.lock() else {
            return Vec::new();
        };
        loader.render(width)
    }

    fn invalidate(&mut self) {
        if let Ok(mut loader) = self.0.lock() {
            loader.invalidate();
        }
    }
}

/// Width-aware preview truncation with caching (bash-execution.ts:153-166):
/// re-truncates only when the render width changes.
struct CachedVisualTruncation {
    text: String,
    max_visual_lines: usize,
    padding_x: usize,
    cache: RefCell<Option<(usize, Vec<String>)>>,
}

impl Component for CachedVisualTruncation {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = self.cache.borrow().as_ref() {
            if *cached_width == width {
                return lines.clone();
            }
        }
        let result =
            truncate_to_visual_lines(&self.text, self.max_visual_lines, width, self.padding_x);
        *self.cache.borrow_mut() = Some((width, result.visual_lines.clone()));
        result.visual_lines
    }

    fn invalidate(&mut self) {
        *self.cache.borrow_mut() = None;
    }
}

/// Component for displaying bash command execution with streaming output
/// (bash-execution.ts:22-220).
pub struct BashExecutionComponent {
    command: String,
    output_lines: Vec<String>,
    status: BashStatus,
    exit_code: Option<i32>,
    loader: Arc<Mutex<Loader>>,
    truncation_result: Option<TruncationResult>,
    full_output_path: Option<String>,
    expanded: bool,
    content_container: Container,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
    theme: Arc<Theme>,
}

impl BashExecutionComponent {
    pub fn new(
        command: impl Into<String>,
        render_handle: RenderHandle,
        theme: Arc<Theme>,
        exclude_from_context: bool,
    ) -> Self {
        let command = command.into();

        // Use dim border for excluded-from-context commands (!! prefix),
        // bash-execution.ts:36-38.
        let color_key = if exclude_from_context {
            "dim"
        } else {
            "bashMode"
        };
        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |str: &str| theme.fg(color_key, str))
        };

        // Loader (bash-execution.ts:54-61).
        let loader_message = format!("Running... ({} to cancel)", key_text("tui.select.cancel"));
        let loader = Loader::new(
            render_handle,
            {
                let theme = Arc::clone(&theme);
                move |spinner: &str| theme.fg(color_key, spinner)
            },
            {
                let theme = Arc::clone(&theme);
                move |text: &str| theme.fg("muted", text)
            },
            loader_message,
            None,
        );

        let mut content_container = Container::new();

        // Command header (bash-execution.ts:50-52). Note: `updateDisplay`
        // re-adds the header with the `bashMode` color unconditionally
        // (bash-execution.ts:138).
        let header = Text::new(
            theme.fg(color_key, &Theme::bold(&format!("$ {command}"))),
            1,
            0,
            None,
        );
        content_container.add_child(Box::new(header));

        // Loader
        let loader = Arc::new(Mutex::new(loader));
        content_container.add_child(Box::new(LoaderRef(Arc::clone(&loader))));

        // Add spacer + top/bottom borders around the content container
        // (bash-execution.ts:40-64).
        let top_border = DynamicBorder::new(border_color.clone());
        let bottom_border = DynamicBorder::new(border_color);

        Self {
            command,
            output_lines: Vec::new(),
            status: BashStatus::Running,
            exit_code: None,
            loader,
            truncation_result: None,
            full_output_path: None,
            expanded: false,
            content_container,
            top_border,
            bottom_border,
            theme,
        }
    }

    /// `setExpanded` (bash-execution.ts:70-73).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    /// `appendOutput` (bash-execution.ts:80-96): strip ANSI codes and
    /// normalize line endings, appending to the output lines.
    pub fn append_output(&mut self, chunk: &str) {
        // Note: binary data is already sanitized in the bash executor
        // (bash-execution.ts:83).
        let clean = strip_ansi(chunk).replace("\r\n", "\n").replace('\r', "\n");

        let new_lines: Vec<&str> = clean.split('\n').collect();
        if !self.output_lines.is_empty() && !new_lines.is_empty() {
            // Append first chunk to last line (incomplete line continuation)
            // (bash-execution.ts:87-90).
            let last = self.output_lines.pop().expect("non-empty checked");
            self.output_lines.push(last + new_lines[0]);
            self.output_lines
                .extend(new_lines[1..].iter().map(|s| s.to_string()));
        } else {
            self.output_lines
                .extend(new_lines.iter().map(|s| s.to_string()));
        }

        self.update_display();
    }

    /// `setComplete` (bash-execution.ts:98-117).
    pub fn set_complete(
        &mut self,
        exit_code: Option<i32>,
        cancelled: bool,
        truncation_result: Option<TruncationResult>,
        full_output_path: Option<String>,
    ) {
        self.exit_code = exit_code;
        self.status = if cancelled {
            BashStatus::Cancelled
        } else if matches!(exit_code, Some(code) if code != 0) {
            BashStatus::Error
        } else {
            BashStatus::Complete
        };
        self.truncation_result = truncation_result;
        self.full_output_path = full_output_path;

        // Stop loader (bash-execution.ts:114).
        if let Ok(mut loader) = self.loader.lock() {
            loader.stop();
        }

        self.update_display();
    }

    /// `updateDisplay` (bash-execution.ts:119-205).
    fn update_display(&mut self) {
        // Apply truncation for LLM context limits (same limits as bash tool,
        // bash-execution.ts:121-125).
        let full_output = self.output_lines.join("\n");
        let context_truncation = truncate_tail(
            &full_output,
            Some(TruncateOptions {
                max_lines: DEFAULT_MAX_LINES,
                max_bytes: DEFAULT_MAX_BYTES,
            }),
        );

        // Get the lines to potentially display (after context truncation)
        // (bash-execution.ts:128).
        let available_lines: Vec<&str> = if context_truncation.content.is_empty() {
            Vec::new()
        } else {
            context_truncation.content.split('\n').collect()
        };

        // Apply preview truncation based on expanded state
        // (bash-execution.ts:131-132).
        let preview_start = available_lines.len().saturating_sub(PREVIEW_LINES);
        let preview_logical_lines = &available_lines[preview_start..];
        let hidden_line_count = available_lines.len() - preview_logical_lines.len();

        // Rebuild content container (bash-execution.ts:135-136).
        self.content_container.clear();

        // Command header (bash-execution.ts:138-139).
        let header = Text::new(
            self.theme
                .fg("bashMode", &Theme::bold(&format!("$ {}", self.command))),
            1,
            0,
            None,
        );
        self.content_container.add_child(Box::new(header));

        // Output (bash-execution.ts:142-168).
        if !available_lines.is_empty() {
            if self.expanded {
                // Show all lines.
                let display_text = available_lines
                    .iter()
                    .map(|line| self.theme.fg("muted", line))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.content_container.add_child(Box::new(Text::new(
                    format!("\n{display_text}"),
                    1,
                    0,
                    None,
                )));
            } else {
                // Use shared visual truncation utility with width-aware
                // caching (bash-execution.ts:149-166).
                let styled_output = preview_logical_lines
                    .iter()
                    .map(|line| self.theme.fg("muted", line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let styled_input = format!("\n{styled_output}");
                self.content_container
                    .add_child(Box::new(CachedVisualTruncation {
                        text: styled_input,
                        max_visual_lines: PREVIEW_LINES,
                        padding_x: 1,
                        cache: RefCell::new(None),
                    }));
            }
        }

        // Loader or status (bash-execution.ts:170-204).
        if self.status == BashStatus::Running {
            self.content_container
                .add_child(Box::new(LoaderRef(Arc::clone(&self.loader))));
        } else {
            let mut status_parts: Vec<String> = Vec::new();

            // Show how many lines are hidden (collapsed preview)
            // (bash-execution.ts:176-187).
            if hidden_line_count > 0 {
                if self.expanded {
                    status_parts.push(format!(
                        "{}{}{}",
                        self.theme.fg("muted", "("),
                        key_hint(&self.theme, "app.tools.expand", "to collapse"),
                        self.theme.fg("muted", ")"),
                    ));
                } else {
                    status_parts.push(format!(
                        "{}{}{}",
                        self.theme
                            .fg("muted", &format!("... {hidden_line_count} more lines ("),),
                        key_hint(&self.theme, "app.tools.expand", "to expand"),
                        self.theme.fg("muted", ")"),
                    ));
                }
            }

            if self.status == BashStatus::Cancelled {
                status_parts.push(self.theme.fg("warning", "(cancelled)"));
            } else if self.status == BashStatus::Error {
                status_parts.push(self.theme.fg(
                    "error",
                    &format!("(exit {})", self.exit_code.unwrap_or_default()),
                ));
            }

            // Add truncation warning (context truncation, not preview
            // truncation) (bash-execution.ts:195-199).
            let was_truncated = self.truncation_result.as_ref().is_some_and(|t| t.truncated)
                || context_truncation.truncated;
            if was_truncated {
                if let Some(path) = &self.full_output_path {
                    status_parts.push(
                        self.theme
                            .fg("warning", &format!("Output truncated. Full output: {path}")),
                    );
                }
            }

            if !status_parts.is_empty() {
                self.content_container.add_child(Box::new(Text::new(
                    format!("\n{}", status_parts.join("\n")),
                    1,
                    0,
                    None,
                )));
            }
        }
    }

    /// `getOutput` (bash-execution.ts:210-212).
    pub fn get_output(&self) -> String {
        self.output_lines.join("\n")
    }

    /// `getCommand` (bash-execution.ts:217-219).
    pub fn get_command(&self) -> &str {
        &self.command
    }
}

impl Component for BashExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // Container children: [Spacer, top border, content, bottom border]
        // (bash-execution.ts:40-64).
        let mut lines = vec![String::new()];
        lines.extend(self.top_border.render(width));
        lines.extend(self.content_container.render(width));
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.bottom_border.invalidate();
        self.content_container.invalidate();
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
    use rpi_tui::tui::RenderHandle;
    use rpi_tui::utils::visible_width;

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
    }

    fn component(exclude: bool) -> BashExecutionComponent {
        BashExecutionComponent::new("pwd", RenderHandle::new(|| {}), theme(), exclude)
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
    fn renders_borders_and_command() {
        let component = component(false);
        let lines = component.render(40);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("$ pwd"));
        assert!(lines.iter().any(|l| l.contains('─')));
    }

    #[test]
    fn appends_streaming_output_and_completes() {
        let mut component = component(false);
        component.append_output("line1\n");
        component.append_output("line2\n");
        // The trailing newline of the last chunk stays in the output lines
        // (bash-execution.ts:80-96), so getOutput keeps it.
        assert_eq!(component.get_output(), "line1\nline2\n");
        component.set_complete(Some(0), false, None, None);
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("line1"));
        assert!(stripped.contains("line2"));
        // No cancel hint / exit code for a clean exit.
        assert!(!stripped.contains("(exit"));
    }

    #[test]
    fn error_status_shows_exit_code() {
        let mut component = component(false);
        component.append_output("boom");
        component.set_complete(Some(2), false, None, None);
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("(exit 2)"));
    }

    #[test]
    fn cancelled_status_shows_marker() {
        let mut component = component(false);
        component.append_output("partial");
        component.set_complete(Some(0), true, None, None);
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("(cancelled)"));
    }

    #[test]
    fn strips_ansi_and_normalizes_line_endings() {
        let mut component = component(false);
        component.append_output("\u{1b}[31mred\u{1b}[0m\r\nnext\r");
        assert_eq!(component.get_output(), "red\nnext\n");
    }

    #[test]
    fn collapsed_preview_respects_render_width() {
        // Upstream regression test bash-execution-width.test.ts (#2569):
        // collapsed preview lines must respect the render-time width.
        let mut component = component(false);
        let long_line = "x".repeat(150);
        component.append_output(&format!("{long_line}\n{long_line}\n"));
        component.set_complete(Some(0), false, None, None);

        let narrow_width = 80;
        let lines = component.render(narrow_width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w <= narrow_width,
                "Line {i} visibleWidth={w} > {narrow_width}"
            );
        }

        // Re-computes when the width changes between renders.
        let _wide = component.render(200);
        let lines60 = component.render(60);
        for line in &lines60 {
            assert!(visible_width(line) <= 60);
        }
    }

    #[test]
    fn preview_hides_lines_with_hint() {
        let mut component = component(false);
        for i in 0..30 {
            component.append_output(&format!("line {i}\n"));
        }
        component.set_complete(Some(0), false, None, None);
        let stripped = strip_ansi(&component.render(100).join("\n"));
        // 30 content lines + the trailing empty line of the last chunk:
        // 31 available lines, 20 shown -> "... 11 more lines (ctrl+o to
        // expand)".
        assert!(stripped.contains("... 11 more lines ("));
        assert!(stripped.contains("ctrl+o"));
        assert!(stripped.contains("to expand"));
    }

    #[test]
    fn expanded_shows_all_lines() {
        let mut component = component(false);
        for i in 0..30 {
            component.append_output(&format!("line {i}\n"));
        }
        component.set_complete(Some(0), false, None, None);
        component.set_expanded(true);
        let stripped = strip_ansi(&component.render(100).join("\n"));
        assert!(stripped.contains("line 29"));
        assert!(stripped.contains("to collapse"));
    }

    #[test]
    fn truncation_warning_with_full_output_path() {
        let mut component = component(false);
        component.append_output("some output");
        let truncated = TruncationResult {
            content: "some output".into(),
            truncated: true,
            truncated_by: None,
            total_lines: 0,
            total_bytes: 0,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines: 0,
            max_bytes: 0,
        };
        component.set_complete(
            Some(0),
            false,
            Some(truncated),
            Some("/tmp/full.log".into()),
        );
        let stripped = strip_ansi(&component.render(100).join("\n"));
        assert!(stripped.contains("Output truncated. Full output: /tmp/full.log"));
    }

    #[test]
    fn excluded_from_context_uses_dim_border() {
        let excluded = component(true);
        let normal = component(false);
        let excluded_border = excluded
            .render(40)
            .iter()
            .find(|l| l.contains('─'))
            .unwrap()
            .clone();
        let normal_border = normal
            .render(40)
            .iter()
            .find(|l| l.contains('─'))
            .unwrap()
            .clone();
        // Both borders are colored; the dim border differs from bashMode.
        assert!(excluded_border.contains("\u{1b}[38;"));
        assert_ne!(excluded_border, normal_border);
    }
}
