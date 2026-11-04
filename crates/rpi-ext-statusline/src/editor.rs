//! `/statusline` editor and non-TUI output (TE52 R20 §4.5 / 04 §6).
//!
//! TUI: a line-frame component (ADR-0024) with three pages — fields,
//! options and preview. Selection/order/options only mutate a draft; saving
//! goes through the A+B coordinator ([`crate::settings_write`]) and only a
//! successful write swaps the running configuration (the refresh loop
//! reconciles subscriptions and re-renders immediately).
//!
//! RPC / `--mode json` / print: never open the editor or write files; emit
//! the current configuration, help or save guidance through `ui.emitText`
//! (validated first). NullUi/subagents answer `unsupported-output`.
//!
//! The editor keeps the original `statusLine` JSON so `command`, `timeoutMs`
//! and unknown keys/ids survive a save; only the items keys are replaced.

use std::path::PathBuf;

use chrono::Utc;
use rpi_ext_sdk::interactive_ui::{
    self, Component, ComponentCursor, HostCall, InteractiveUiError, MountOptions, OverlayAnchor,
    OverlayOptions, SizeValue, run_component,
};
use rpi_ext_sdk::output;
use rpi_tui::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};
use serde_json::{Map, Value, json};

use crate::AsyncHostCalls;
use crate::config::{self, StatusLine};
use crate::fields::{self, ContextInfo, FieldData, FieldId, ModelInfo, Palette};
use crate::items::{ItemEntry, ItemsConfig};
use crate::session_stats::SessionStats;
use crate::settings_write;

/// Transport adapter: the shared trampoline → the SDK `HostCall` shape.
#[derive(Clone, Copy)]
struct EditorHost {
    calls: AsyncHostCalls,
}

impl HostCall for EditorHost {
    fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
        let response = crate::host_call_static(&self.calls, method, args);
        if let Some(error) = response.get("error") {
            let kind = error.get("kind").and_then(Value::as_str).unwrap_or("call");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("host call failed");
            return Err(InteractiveUiError::from_host_error(kind, message));
        }
        Ok(response.get("ok").cloned().unwrap_or(Value::Null))
    }
}

/// One page of the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Fields,
    Options,
    Preview,
}

/// Editor state machine (04 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Editing,
    Previewing,
    Saving,
    Applied,
}

const OPTION_ROWS: usize = 5;

/// The editor draft and its component state.
struct Editor {
    host: EditorHost,
    path: PathBuf,
    /// `statusLine` JSON captured when the editor opened (unknown keys and
    /// the retained command survive the save).
    opened: Map<String, Value>,
    /// Fingerprint recorded at open (conflict detection).
    fingerprint: String,
    draft: ItemsConfig,
    /// `type:"command"` at open → the save hint warns about the switch.
    command_mode: bool,
    data: FieldData,
    page: Page,
    cursor: usize,
    option_cursor: usize,
    /// Terminal/content width from the host resize event (0 before the first
    /// render).
    width: usize,
    /// Maximum frame height from `maxHeight` (the host reports the resolved
    /// budget through `resize`; `None` before the first event).
    height: Option<usize>,
    /// First visible field/option row (the dialog window follows the cursor).
    scroll: usize,
    state: State,
    message: Option<String>,
    reset_armed: bool,
    done: Option<Value>,
}

impl Editor {
    /// Rows shown on the fields page: selected known fields in draft order,
    /// then unknown (preserved) ids, then unselected known fields in registry
    /// order.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for entry in &self.draft.entries {
            if let Some(field) = entry.field {
                rows.push(Row::Known {
                    field,
                    selected: true,
                });
            }
        }
        for raw in &self.draft.unknown {
            rows.push(Row::Unknown { raw: raw.clone() });
        }
        for field in fields::ALL_FIELDS {
            if !self.draft.has_field(field) {
                rows.push(Row::Known {
                    field,
                    selected: false,
                });
            }
        }
        rows
    }

    fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(len - 1);
        }
    }

    fn toggle(&mut self) {
        let rows = self.rows();
        let Some(Row::Known { field, selected }) = rows.get(self.cursor) else {
            return;
        };
        let field = *field;
        let selected = *selected;
        if selected {
            self.draft
                .entries
                .retain(|entry| entry.field != Some(field));
        } else {
            self.draft.entries.push(ItemEntry {
                raw: field.id().to_owned(),
                field: Some(field),
            });
        }
        self.clamp_cursor();
        self.message = None;
    }

    fn move_selected(&mut self, up: bool) {
        let rows = self.rows();
        let Some(Row::Known { field, selected }) = rows.get(self.cursor) else {
            return;
        };
        if !*selected {
            return;
        }
        let field = *field;
        let Some(index) = self
            .draft
            .entries
            .iter()
            .position(|entry| entry.field == Some(field))
        else {
            return;
        };
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|target| *target < self.draft.entries.len())
        };
        let Some(target) = target else {
            return;
        };
        self.draft.entries.swap(index, target);
        self.cursor = target;
        self.message = None;
    }

    fn reset(&mut self) {
        let defaults = ItemsConfig::default();
        self.draft = ItemsConfig {
            unknown: std::mem::take(&mut self.draft.unknown),
            ..defaults
        };
        self.cursor = 0;
        self.option_cursor = 0;
        self.reset_armed = false;
        self.message = Some("reset to defaults (press s to save)".to_owned());
    }

    fn save(&mut self) {
        self.state = State::Saving;
        let object = self.merged_status_line();
        match settings_write::save_status_line(
            &self.path,
            &self.fingerprint,
            &Value::Object(object),
        ) {
            Ok(()) => {
                self.state = State::Applied;
                self.message = Some("saved".to_owned());
                crate::request_refresh("config_saved");
                self.done = Some(json!({"action": "saved"}));
            }
            Err(error) => {
                self.state = State::Editing;
                self.message = Some(error.message.clone());
            }
        }
    }

    fn merged_status_line(&self) -> Map<String, Value> {
        let mut object = self.opened.clone();
        for (key, value) in self.draft.to_settings_object() {
            object.insert(key, value);
        }
        object
    }

    fn cycle_option(&mut self, forward: bool) {
        match self.option_cursor {
            0 => self.draft.use_colors = !self.draft.use_colors,
            1 => {
                self.draft.number_format = match self.draft.number_format {
                    fields::NumberFormat::Compact => fields::NumberFormat::Exact,
                    fields::NumberFormat::Exact => fields::NumberFormat::Compact,
                }
            }
            2 => {
                // Separator presets; Enter opens a free-text input.
                const SEPARATORS: [&str; 4] = [" · ", " | ", " ", " · · "];
                let current = self.draft.separator.as_str();
                let index = SEPARATORS.iter().position(|sep| *sep == current);
                let next = match (index, forward) {
                    (Some(index), true) => (index + 1) % SEPARATORS.len(),
                    (Some(index), false) => (index + SEPARATORS.len() - 1) % SEPARATORS.len(),
                    (None, _) => 0,
                };
                self.draft.separator = SEPARATORS[next].to_owned();
            }
            3 => {
                self.draft.placement = match (self.draft.placement, forward) {
                    (config::Placement::Widget, true) => config::Placement::Status,
                    (config::Placement::Status, true) => config::Placement::Replace,
                    (config::Placement::Replace, true) => config::Placement::Widget,
                    (config::Placement::Widget, false) => config::Placement::Replace,
                    (config::Placement::Replace, false) => config::Placement::Status,
                    (config::Placement::Status, false) => config::Placement::Widget,
                };
            }
            _ => self.draft.live_tokens.enabled = !self.draft.live_tokens.enabled,
        }
        self.message = None;
    }

    fn edit_separator(&mut self) {
        let placeholder = self.draft.separator.clone();
        match self.host.call(
            "ui.input",
            json!({"title": "statusline separator", "placeholder": placeholder}),
        ) {
            Ok(Value::String(value)) if fields::valid_separator(&value) => {
                self.draft.separator = value;
                self.message = None;
            }
            Ok(Value::String(_)) => {
                self.message =
                    Some("separator rejected: no newlines or control characters".to_owned());
            }
            _ => {}
        }
    }

    fn option_rows(&self) -> Vec<String> {
        vec![
            format!(
                "useColors: {}",
                if self.draft.use_colors {
                    "true"
                } else {
                    "false"
                }
            ),
            format!(
                "numberFormat: {}",
                match self.draft.number_format {
                    fields::NumberFormat::Compact => "compact",
                    fields::NumberFormat::Exact => "exact",
                }
            ),
            format!("separator: {:?}", self.draft.separator),
            format!(
                "placement: {}",
                match self.draft.placement {
                    config::Placement::Replace => "replace",
                    config::Placement::Widget => "widget",
                    config::Placement::Status => "status",
                }
            ),
            format!(
                "liveTokens: {}",
                if self.draft.live_tokens.enabled {
                    "on"
                } else {
                    "off (pending live fields are hidden)"
                }
            ),
        ]
    }

    fn preview_line(&self) -> Option<String> {
        let mut data = self.data.clone();
        data.preview = true;
        data.number_format = self.draft.number_format;
        data.palette = if self.draft.use_colors {
            self.data.palette.clone()
        } else {
            None
        };
        fields::render_line(
            &self.draft.entries,
            &self.draft.separator,
            self.draft.use_colors,
            &data,
        )
    }

    fn placement_hint(&self) -> &'static str {
        match self.draft.placement {
            config::Placement::Replace => {
                "replace hides the built-in footer and rpi-usage (not recommended)"
            }
            config::Placement::Widget => {
                "widget keeps the built-in footer and rpi-usage (recommended)"
            }
            config::Placement::Status => {
                "status shares the host status row: dot-separated, auto-wrap"
            }
        }
    }

    fn status_line(&self) -> String {
        let mut parts = vec![
            match self.state {
                State::Editing => "editing",
                State::Previewing => "preview",
                State::Saving => "saving",
                State::Applied => "saved",
            }
            .to_owned(),
        ];
        if self.command_mode {
            parts.push("saving switches type:command → items".to_owned());
        }
        if let Some(message) = &self.message {
            parts.push(message.clone());
        }
        if self.reset_armed {
            parts.push("reset: press r again to confirm, any other key cancels".to_owned());
        }
        parts.join(" · ")
    }

    /// Rows available to the page body after the frame chrome.
    fn body_budget(&self, boxed: bool) -> usize {
        // Boxed: top, tabs, rule, column header, footer rule, status, hint,
        // bottom. Unboxed: tabs, rule, header, footer rule, status, hint.
        let chrome = if boxed { 8 } else { 7 };
        match self.height {
            Some(height) if height > 0 => height.saturating_sub(chrome).max(1),
            _ => usize::MAX,
        }
    }

    /// Visible window `(start, end)` for `total` rows, keeping `cursor` in
    /// view. The stored scroll offset follows the window so repeated renders
    /// are stable.
    fn window(&mut self, total: usize, budget: usize, cursor: usize) -> (usize, usize) {
        if budget >= total {
            self.scroll = 0;
            return (0, total);
        }
        let cursor = cursor.min(total.saturating_sub(1));
        let mut start = self.scroll.min(total.saturating_sub(budget));
        if cursor < start {
            start = cursor;
        } else if cursor >= start + budget {
            start = cursor + 1 - budget;
        }
        self.scroll = start;
        (start, (start + budget).min(total))
    }

    fn tabs_line(&self, palette: Option<&fields::Palette>) -> String {
        let mut parts = Vec::new();
        for (page, label) in [
            (Page::Fields, format!("Fields {}", self.draft.entries.len())),
            (Page::Options, "Options".to_owned()),
            (Page::Preview, "Preview".to_owned()),
        ] {
            if page == self.page {
                parts.push(paint_bold(
                    palette,
                    fields::Tone::Accent,
                    &format!("[{label}]"),
                ));
            } else {
                parts.push(paint(palette, fields::Tone::Muted, &label));
            }
        }
        parts.join("  ")
    }
}

enum Row {
    Known { field: FieldId, selected: bool },
    Unknown { raw: String },
}

/// Right-align `suffix` in a `total`-wide header when there is room.
fn right_align(header: &str, suffix: &str, total: usize) -> String {
    let header_width = visible_width(header);
    let suffix_width = visible_width(suffix);
    if header_width + suffix_width + 1 > total {
        return header.to_owned();
    }
    format!(
        "{header}{}{suffix}",
        " ".repeat(total - header_width - suffix_width)
    )
}

/// ANSI painter for the dialog chrome. SGR parameters come from the host
/// theme palette (the same model as the field values), so the editor and the
/// rendered statusline stay visually consistent.
fn paint(palette: Option<&fields::Palette>, tone: fields::Tone, text: &str) -> String {
    match palette.and_then(|palette| palette.sgr(tone)) {
        Some(sgr) => format!("\x1b[{sgr}m{text}\x1b[0m"),
        None => text.to_owned(),
    }
}

fn paint_bold(palette: Option<&fields::Palette>, tone: fields::Tone, text: &str) -> String {
    match palette.and_then(|palette| palette.sgr(tone)) {
        Some(sgr) => format!("\x1b[{sgr};1m{text}\x1b[0m"),
        None => text.to_owned(),
    }
}

/// Selected-row paint: the theme's `selectedBg` when available, reverse video
/// otherwise. Without a palette the row stays plain (the `>` marker still
/// identifies the cursor).
fn paint_selected(palette: Option<&fields::Palette>, text: &str) -> String {
    if let Some(background) = palette.and_then(|palette| palette.selected_bg_sgr()) {
        format!("\x1b[{background}m{text}\x1b[0m")
    } else if palette.is_some() {
        format!("\x1b[7m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn box_top(width: usize, title: &str, palette: Option<&fields::Palette>) -> String {
    if width < 4 {
        return paint(palette, fields::Tone::Info, &"─".repeat(width));
    }
    let title = format!(" {title} ");
    let title_width = visible_width(&title);
    let fill = width.saturating_sub(3 + title_width);
    if fill == 0 {
        return format!(
            "{}{}{}",
            paint(palette, fields::Tone::Info, "┌─"),
            paint_bold(palette, fields::Tone::Accent, &title),
            paint(palette, fields::Tone::Info, "┐")
        );
    }
    format!(
        "{}{}{}{}",
        paint(palette, fields::Tone::Info, "┌─"),
        paint_bold(palette, fields::Tone::Accent, &title),
        paint(palette, fields::Tone::Info, &"─".repeat(fill)),
        paint(palette, fields::Tone::Info, "┐")
    )
}

fn box_bottom(width: usize, palette: Option<&fields::Palette>) -> String {
    if width < 4 {
        return paint(palette, fields::Tone::Info, &"─".repeat(width));
    }
    format!(
        "{}{}{}",
        paint(palette, fields::Tone::Info, "└"),
        paint(
            palette,
            fields::Tone::Info,
            &"─".repeat(width.saturating_sub(2))
        ),
        paint(palette, fields::Tone::Info, "┘")
    )
}

fn rule_line(width: usize, boxed: bool, palette: Option<&fields::Palette>) -> String {
    if boxed && width >= 4 {
        format!(
            "{}{}{}",
            paint(palette, fields::Tone::Info, "├"),
            paint(
                palette,
                fields::Tone::Info,
                &"─".repeat(width.saturating_sub(2))
            ),
            paint(palette, fields::Tone::Info, "┤")
        )
    } else {
        paint(palette, fields::Tone::Info, &"─".repeat(width.max(1)))
    }
}

/// One content row inside the optional box: `│ ` + padded body + ` │`.
fn content_line(
    width: usize,
    boxed: bool,
    palette: Option<&fields::Palette>,
    text: &str,
) -> String {
    let inner = if boxed {
        width.saturating_sub(4)
    } else {
        width.max(1)
    };
    let body = truncate_to_width(text, inner, "…", true);
    if boxed {
        format!(
            "{} {} {}",
            paint(palette, fields::Tone::Info, "│"),
            body,
            paint(palette, fields::Tone::Info, "│")
        )
    } else {
        body
    }
}

impl Component for Editor {
    fn render(&mut self, width: usize) -> Vec<String> {
        // The host normally delivers a resize before the first render; a
        // zero-width frame (scripted mounts/tests) still needs a readable
        // default so the title and content are present.
        let width = if width == 0 { 80 } else { width };
        self.width = width;
        let boxed = width >= 24 && self.height.is_none_or(|height| height >= 10);
        let inner = if boxed {
            width.saturating_sub(4)
        } else {
            width.max(1)
        };
        let budget = self.body_budget(boxed);
        let palette = if self.draft.use_colors {
            self.data.palette.clone()
        } else {
            None
        };
        let palette = palette.as_ref();
        let mut lines = Vec::new();
        if boxed {
            lines.push(box_top(width, "rpi-statusline", palette));
        }
        lines.push(content_line(
            width,
            boxed,
            palette,
            &self.tabs_line(palette),
        ));
        lines.push(rule_line(width, boxed, palette));

        let id_width = 24.min(inner.saturating_sub(12)).max(8);
        let mut data = self.data.clone();
        data.preview = true;
        data.number_format = self.draft.number_format;
        data.palette = palette.cloned();

        match self.page {
            Page::Fields => {
                let rows = self.rows();
                let total = rows.len();
                let (start, end) = self.window(total, budget, self.cursor);
                let range = if total == 0 {
                    "0/0".to_owned()
                } else {
                    format!("{}-{}/{}", start + 1, end, total)
                };
                let header = format!("  {:<id_width$} VALUE", "FIELD");
                let header = right_align(&header, &range, inner);
                lines.push(content_line(
                    width,
                    boxed,
                    palette,
                    &paint(palette, fields::Tone::Muted, &header),
                ));
                for (index, row) in rows
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(end.saturating_sub(start))
                {
                    let at_cursor = index == self.cursor;
                    let (check, id, value, tone, selected) = match row {
                        Row::Known { field, selected } => {
                            let value = fields::value(*field, &data)
                                .map(|value| value.text)
                                .unwrap_or_else(|| fields::placeholder(*field));
                            (
                                if *selected { "[x]" } else { "[ ]" }.to_owned(),
                                field.id().to_owned(),
                                value,
                                if *selected {
                                    fields::Tone::Accent
                                } else {
                                    fields::Tone::Plain
                                },
                                *selected,
                            )
                        }
                        Row::Unknown { raw } => (
                            "[?]".to_owned(),
                            raw.clone(),
                            "(unknown id preserved)".to_owned(),
                            fields::Tone::Warning,
                            true,
                        ),
                    };
                    let marker = if at_cursor { ">" } else { " " };
                    let id_padded = format!("{id:<id_width$}");
                    let plain = format!("{marker} {check} {id_padded} {value}");
                    let line = if at_cursor {
                        paint_selected(palette, &truncate_to_width(&plain, inner, "…", true))
                    } else {
                        format!(
                            "{} {} {} {}",
                            paint(palette, fields::Tone::Muted, marker),
                            paint(
                                palette,
                                if selected {
                                    fields::Tone::Success
                                } else {
                                    fields::Tone::Muted
                                },
                                &check,
                            ),
                            paint(palette, tone, &id_padded),
                            paint(palette, fields::Tone::Muted, &value),
                        )
                    };
                    lines.push(content_line(width, boxed, palette, &line));
                }
            }
            Page::Options => {
                let rows = self.option_rows();
                let total = rows.len();
                let (start, end) = self.window(total, budget, self.option_cursor);
                let range = format!("{}-{}/{}", start + 1, end, total);
                let header = format!("  {:<20} VALUE", "OPTION");
                let header = right_align(&header, &range, inner);
                lines.push(content_line(
                    width,
                    boxed,
                    palette,
                    &paint(palette, fields::Tone::Muted, &header),
                ));
                for (index, row) in rows
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(end.saturating_sub(start))
                {
                    let at_cursor = index == self.option_cursor;
                    let (name, value) = row.split_once(':').unwrap_or((row.as_str(), ""));
                    let name = format!("{name:<20}");
                    let plain = format!("{} {name} {value}", if at_cursor { ">" } else { " " });
                    let line = if at_cursor {
                        paint_selected(palette, &truncate_to_width(&plain, inner, "…", true))
                    } else {
                        format!(
                            "{} {} {}",
                            paint(palette, fields::Tone::Muted, " "),
                            paint(palette, fields::Tone::Accent, &name),
                            paint(palette, fields::Tone::Muted, value),
                        )
                    };
                    lines.push(content_line(width, boxed, palette, &line));
                }
            }
            Page::Preview => {
                lines.push(content_line(
                    width,
                    boxed,
                    palette,
                    &paint(
                        palette,
                        fields::Tone::Muted,
                        "  PREVIEW · real data; missing values show placeholders",
                    ),
                ));
                let mut preview = match self.preview_line() {
                    Some(line) if !line.is_empty() => wrap_text_with_ansi(&line, inner),
                    _ => vec!["(no custom output — the built-in footer is restored)".to_owned()],
                };
                preview.push(String::new());
                preview.extend(wrap_text_with_ansi(self.placement_hint(), inner));
                if self.draft.has_live_field() && !self.draft.live_tokens.enabled {
                    preview.push(
                        "live fields are selected but liveTokens is off (pending preset)"
                            .to_owned(),
                    );
                }
                // The PREVIEW header occupies one body row, so the
                // remaining budget is one smaller (keeps the frame at most
                // exactly the host's maxHeight).
                let preview_budget = budget.saturating_sub(1).max(1);
                for line in preview.into_iter().take(preview_budget) {
                    let tone = if line.contains("liveTokens is off") {
                        fields::Tone::Warning
                    } else if line.starts_with('(') {
                        fields::Tone::Muted
                    } else {
                        fields::Tone::Plain
                    };
                    lines.push(content_line(
                        width,
                        boxed,
                        palette,
                        &paint(palette, tone, &line),
                    ));
                }
            }
        }

        lines.push(rule_line(width, boxed, palette));
        let status = self.status_line();
        let status_tone = if self.message.is_some() || self.reset_armed {
            fields::Tone::Warning
        } else {
            match self.state {
                State::Applied => fields::Tone::Success,
                State::Saving => fields::Tone::Warning,
                State::Editing | State::Previewing => fields::Tone::Muted,
            }
        };
        lines.push(content_line(
            width,
            boxed,
            palette,
            &paint(palette, status_tone, &status),
        ));
        lines.push(content_line(
            width,
            boxed,
            palette,
            &paint(
                palette,
                fields::Tone::Muted,
                "↑/↓ move · space toggle · K/J reorder · tab page · s save · r reset · q cancel",
            ),
        ));
        if boxed {
            lines.push(box_bottom(width, palette));
        }
        lines
    }

    fn on_resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = Some(height);
    }

    fn handle_input(&mut self, data: &str) {
        if self.reset_armed {
            if data == "r" {
                self.reset();
                return;
            }
            self.reset_armed = false;
        }
        match data {
            "\x1b[A" | "k" => match self.page {
                Page::Fields => {
                    self.cursor = self.cursor.saturating_sub(1);
                }
                Page::Options => {
                    self.option_cursor = self.option_cursor.saturating_sub(1);
                }
                Page::Preview => {}
            },
            "\x1b[B" | "j" => {
                let max = match self.page {
                    Page::Fields => self.rows().len().saturating_sub(1),
                    Page::Options => OPTION_ROWS - 1,
                    Page::Preview => 0,
                };
                match self.page {
                    Page::Fields => self.cursor = (self.cursor + 1).min(max),
                    Page::Options => self.option_cursor = (self.option_cursor + 1).min(max),
                    Page::Preview => {}
                }
            }
            "\x1b[1;2A" | "K" => {
                if self.page == Page::Fields {
                    self.move_selected(true);
                }
            }
            "\x1b[1;2B" | "J" => {
                if self.page == Page::Fields {
                    self.move_selected(false);
                }
            }
            "\t" => {
                self.page = match self.page {
                    Page::Fields => Page::Options,
                    Page::Options => Page::Preview,
                    Page::Preview => Page::Fields,
                };
                self.state = if self.page == Page::Preview {
                    State::Previewing
                } else {
                    State::Editing
                };
                self.message = None;
            }
            " " | "\r" | "\n" => match self.page {
                Page::Fields => self.toggle(),
                Page::Options => {
                    if self.option_cursor == 2 {
                        self.edit_separator();
                    } else {
                        self.cycle_option(true);
                    }
                }
                Page::Preview => self.page = Page::Fields,
            },
            "\x1b[C" | "l" => {
                if self.page == Page::Options {
                    self.cycle_option(true);
                }
            }
            "\x1b[D" | "h" => {
                if self.page == Page::Options {
                    self.cycle_option(false);
                }
            }
            "s" => self.save(),
            "r" => {
                self.reset_armed = true;
                self.message = None;
            }
            "q" | "\x1b" => {
                self.done = Some(json!({"action": "cancelled"}));
            }
            _ => {}
        }
    }

    fn cursor(&self) -> Option<ComponentCursor> {
        None
    }

    fn done(&mut self) -> Option<Value> {
        self.done.take()
    }
}

/// Fetch the preview data synchronously (command/editor path; never the
/// refresh loop).
fn fetch_data(
    host: &EditorHost,
    use_colors: bool,
    engine_git: Option<crate::git::GitInfo>,
) -> FieldData {
    let value = |method: &str| {
        host.call(method, json!({}))
            .ok()
            .filter(|value| !value.is_null())
    };
    let model = value("ctx.model").and_then(|model| {
        let id = model.get("id").and_then(Value::as_str).map(str::to_owned);
        let name = model.get("name").and_then(Value::as_str).map(str::to_owned);
        id.map(|id| ModelInfo { id, name })
    });
    let context = value("ctx.getContextUsage").map(|usage| ContextInfo {
        tokens: usage.get("tokens").and_then(Value::as_u64),
        context_window: usage
            .get("contextWindow")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        percent: usage.get("percent").and_then(Value::as_f64),
    });
    let stats = value("ctx.sessionStats")
        .and_then(|value| serde_json::from_value::<SessionStats>(value).ok());
    let live = crate::with_engine(|engine| {
        let now = std::time::Instant::now();
        engine.live_peek(now)
    })
    .map(|peek| peek.into_view(model.as_ref().map(|model| model.id.as_str())));
    let palette = use_colors
        .then(|| value("ui.theme"))
        .flatten()
        .map(|theme| Palette::from_theme(&theme));
    FieldData {
        cwd: value("ctx.cwd").and_then(|value| value.as_str().map(str::to_owned)),
        model,
        thinking: value("getThinkingLevel").and_then(|value| value.as_str().map(str::to_owned)),
        mode: value("getMode").and_then(|value| value.as_str().map(str::to_owned)),
        idle: value("ctx.isIdle").and_then(|value| value.as_bool()),
        context,
        session_name: value("getSessionName").and_then(|value| value.as_str().map(str::to_owned)),
        stats,
        live,
        // The editor preview renders placeholders for missing values; it never
        // shows the footer's held live values.
        live_held: Vec::new(),
        git: engine_git,
        now: Some(Utc::now()),
        number_format: fields::NumberFormat::Compact,
        palette,
        preview: true,
    }
}

fn settings_path() -> PathBuf {
    crate::paths::get_agent_dir().join("settings.json")
}

/// Open the editor on a fresh draft (TUI only).
fn open_editor(calls: &AsyncHostCalls, reset_defaults: bool) -> Value {
    let host = EditorHost { calls: *calls };
    let path = settings_path();
    let (root, fingerprint) = match settings_write::read_status_line(&path) {
        Ok(value) => value,
        Err(error) => {
            emit_or_log(&host, &format!("rpi-statusline: {}", error.message));
            return json!({"error": {"kind": "statusline", "message": error.message}});
        }
    };
    let status_line = root.get("statusLine").cloned().unwrap_or(Value::Null);
    let opened = status_line.as_object().cloned().unwrap_or_default();
    let command_mode = opened.get("type").and_then(Value::as_str) == Some("command");
    let mut draft = parse_draft(&opened);
    if reset_defaults {
        draft = ItemsConfig {
            unknown: std::mem::take(&mut draft.unknown),
            ..ItemsConfig::default()
        };
    }
    let git = crate::with_engine(|engine| engine.git.info().cloned());
    let data = fetch_data(&host, draft.use_colors, git);
    let editor = Editor {
        host,
        path,
        opened,
        fingerprint,
        draft,
        command_mode,
        data,
        page: Page::Fields,
        cursor: 0,
        option_cursor: 0,
        width: 0,
        height: None,
        scroll: 0,
        state: State::Editing,
        message: None,
        reset_armed: false,
        done: None,
    };
    let options = MountOptions {
        overlay: true,
        overlay_options: Some(OverlayOptions {
            anchor: Some(OverlayAnchor::Center),
            width: Some(SizeValue::Percent(80.0)),
            max_height: Some(SizeValue::Percent(70.0)),
            ..OverlayOptions::default()
        }),
        tick_ms: 0,
        keys_when_hidden: Vec::new(),
        cursor: true,
        max_frame_bytes: interactive_ui::DEFAULT_MAX_FRAME_BYTES,
        label: Some("rpi-statusline".to_owned()),
        answerable: None,
    };
    let host = editor.host;
    match run_component(&host, editor, options) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "rpi-statusline editor failed");
            json!({"error": {"kind": "statusline", "message": error.message}})
        }
    }
}

fn parse_draft(opened: &Map<String, Value>) -> ItemsConfig {
    // A command-mode object without an items array starts from the defaults;
    // parse_items already did that. Unknown item ids ride along.
    crate::items::parse_items(opened)
}

/// Emit text when the channel exists; log otherwise (never fail the save
/// path on a notification).
fn emit_or_log(host: &EditorHost, text: &str) {
    if let Err(error) = output::emit_text(host, text, false) {
        tracing::warn!(%error, "rpi-statusline: ui.emitText failed");
    }
}

fn emit_validated(host: &EditorHost, text: &str) -> Result<(), Value> {
    // Validate the channel before producing anything (and before the command
    // would touch the network/files).
    if let Err(error) = output::validate_only(host) {
        return Err(json!({
            "error": {
                "kind": "unsupported-output",
                "message": format!("statusline: {}", error.message),
            }
        }));
    }
    output::emit_text(host, text, false)
        .map_err(|error| json!({"error": {"kind": "statusline", "message": error.message}}))
}

/// Current configuration as concise text for RPC/print.
fn config_text(snapshot: &config::SettingsSnapshot) -> String {
    let Some(status_line) = snapshot.status_line.as_ref() else {
        return "rpi-statusline: not configured.\nEdit in the TUI with /statusline.".to_owned();
    };
    match status_line {
        StatusLine::Command(config) => format!(
            "rpi-statusline: command ({:?})\ncommand: {}\nrefreshInterval: {}\nliveTokens: {}\nEdit in the TUI with /statusline.",
            config.placement,
            config.command,
            config
                .refresh_interval_secs
                .map(|secs| format!("{secs}s"))
                .unwrap_or_else(|| "off".to_owned()),
            if config.live_tokens.enabled {
                "on"
            } else {
                "off"
            },
        ),
        StatusLine::Items(config) => {
            let items = config
                .fields()
                .map(|field| field.id().to_owned())
                .chain(config.unknown.iter().cloned())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "rpi-statusline: items ({:?})\nitems: {items}\nseparator: {:?}\nplacement: {:?}\nnumberFormat: {:?}  useColors: {}  padding: {}\nrefreshInterval: {}s\nliveTokens: {}\nEdit in the TUI with /statusline.",
                config.placement,
                config.separator,
                config.placement,
                config.number_format,
                config.use_colors,
                config.padding,
                config.refresh_interval_secs,
                if config.live_tokens.enabled {
                    "on"
                } else {
                    "off"
                },
            )
        }
    }
}

const HELP: &str = "rpi-statusline\n\
/statusline          open the TUI editor (fields, order, options, preview, save)\n\
/statusline help     show this help\n\
/statusline reset    restore the default items and display options (TUI confirms)\n\
\n\
Fields: model, model-with-reasoning, reasoning, permission-mode, run-state,\n\
current-dir, project-name, git-branch, context-used, context-remaining,\n\
context-window-size, context-tokens, session-name, session-cost,\n\
session-duration, used-tokens, total-input-tokens, total-output-tokens,\n\
cache-read-tokens, cache-write-tokens, streaming, live-output-tokens,\n\
live-output-rate, live-text-tokens, live-thinking-tokens,\n\
live-toolcall-tokens.\n\
\n\
Live tokens prefer provider usage, then the model tokenizer in\n\
~/.rpi/agent/tokenizer, then a CJK-aware estimate (~ prefix).\n\
\n\
Default items: live-output-tokens + live-output-rate only (all other\n\
fields off); adjust them in the editor.\n\
\n\
Editor keys: ↑/↓ move · space toggle · K/J reorder · tab page · s save · r reset · q cancel.";

/// `/statusline` command entry point.
pub fn handle_command(calls: &AsyncHostCalls, args: &str) -> Value {
    let host = EditorHost { calls: *calls };
    let mode = host
        .call("ctx.mode", json!({}))
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    let has_ui = host
        .call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let trimmed = args.trim();
    let tui = mode.as_deref() == Some("tui") && has_ui;
    match trimmed {
        "help" => emit_validated(&host, HELP).map_or_else(|error| error, |_| Value::Null),
        "reset" if tui => {
            // Confirm before writing defaults.
            let confirmed = host
                .call(
                    "ui.confirm",
                    json!({
                        "title": "Reset status line?",
                        "message": "Restore the default realtime-token items (live-output-tokens, live-output-rate) and display options. Other settings are untouched.",
                    }),
                )
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if confirmed {
                open_editor(calls, true)
            } else {
                Value::Null
            }
        }
        "reset" => {
            let snapshot = config::load_settings_snapshot();
            let text = format!(
                "{}\n\nReset requires confirmation in the TUI: run /statusline reset there.",
                config_text(&snapshot)
            );
            emit_validated(&host, &text).map_or_else(|error| error, |_| Value::Null)
        }
        "" if tui => open_editor(calls, false),
        "" => {
            let snapshot = config::load_settings_snapshot();
            emit_validated(&host, &config_text(&snapshot))
                .map_or_else(|error| error, |_| Value::Null)
        }
        other => {
            let text = format!("rpi-statusline: unknown argument `{other}`\n\n{HELP}");
            emit_validated(&host, &text).map_or_else(|error| error, |_| Value::Null)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Placement;

    fn test_host() -> EditorHost {
        EditorHost {
            calls: AsyncHostCalls {
                call: unused_call,
                cookie: 0,
            },
        }
    }

    extern "C" fn unused_call(
        _cookie: rpi_ext_host::native::PluginCookie,
        _request: abi_stable::std_types::RVec<u8>,
    ) -> abi_stable::std_types::RVec<u8> {
        abi_stable::std_types::RVec::new()
    }

    fn editor() -> Editor {
        let opened: Map<String, Value> = serde_json::from_value(json!({
            "type": "items",
            "command": "echo old",
            "timeoutMs": 500,
            "unknownKey": {"a": 1},
            "items": ["session-duration", "live-output-tokens", "legacy-id"],
        }))
        .expect("object");
        Editor {
            host: test_host(),
            path: PathBuf::from("/tmp/does-not-matter.json"),
            fingerprint: "fp".into(),
            draft: parse_draft(&opened),
            command_mode: false,
            data: FieldData {
                cwd: Some("/work/project".into()),
                model: Some(ModelInfo {
                    id: "glm-5.1".into(),
                    name: Some("GLM-5.1".into()),
                }),
                session_name: Some("te52".into()),
                stats: Some(SessionStats {
                    session_id: "s1".into(),
                    session_file: None,
                    started_at: "2026-10-27T10:00:00Z".into(),
                    entry_count: 1,
                    tokens: Default::default(),
                    cost: 0.0,
                }),
                now: Some(Utc::now()),
                ..FieldData::default()
            },
            opened,
            page: Page::Fields,
            cursor: 0,
            option_cursor: 0,
            width: 80,
            height: None,
            scroll: 0,
            state: State::Editing,
            message: None,
            reset_armed: false,
            done: None,
        }
    }

    #[test]
    fn draft_starts_from_the_configured_items_and_unknowns() {
        let editor = editor();
        let fields: Vec<FieldId> = editor.draft.fields().collect();
        assert_eq!(
            fields,
            vec![FieldId::SessionDuration, FieldId::LiveOutputTokens]
        );
        assert_eq!(editor.draft.unknown, vec!["legacy-id"]);
        // Command/timeout/unknown keys survive a save merge.
        let merged = editor.merged_status_line();
        assert_eq!(merged["type"], "items");
        assert_eq!(merged["command"], "echo old");
        assert_eq!(merged["timeoutMs"], 500);
        assert_eq!(merged["unknownKey"]["a"], 1);
        assert_eq!(
            merged["items"],
            json!(["session-duration", "live-output-tokens", "legacy-id"])
        );
    }

    #[test]
    fn toggle_reorder_and_reset_keep_the_draft_consistent() {
        let mut editor = editor();
        // Rows: selected duration, selected live-output, unknown, then the
        // remaining fields in registry order.
        assert_eq!(editor.rows().len(), 26 + 1);
        // Select `model` (cursor at its row).
        let model_row = editor
            .rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    Row::Known {
                        field: FieldId::Model,
                        selected: false
                    }
                )
            })
            .unwrap();
        editor.cursor = model_row;
        editor.toggle();
        assert!(editor.draft.has_field(FieldId::Model));
        // Move it up one slot and observe the new order.
        editor.cursor = 2; // model is appended last
        editor.move_selected(true);
        let fields: Vec<FieldId> = editor.draft.fields().collect();
        assert_eq!(
            fields,
            vec![
                FieldId::SessionDuration,
                FieldId::Model,
                FieldId::LiveOutputTokens
            ]
        );
        // Toggle the duration off.
        editor.cursor = 0;
        editor.toggle();
        assert!(!editor.draft.has_field(FieldId::SessionDuration));
        // Reset restores the default two but preserves unknown ids.
        editor.reset();
        let fields: Vec<FieldId> = editor.draft.fields().collect();
        assert_eq!(
            fields,
            vec![FieldId::LiveOutputTokens, FieldId::LiveOutputRate]
        );
        assert_eq!(editor.draft.unknown, vec!["legacy-id"]);
        assert_eq!(editor.draft.placement, Placement::Status);
    }

    #[test]
    fn options_cycle_only_the_display_values() {
        let mut editor = editor();
        editor.option_cursor = 0;
        editor.cycle_option(true);
        assert!(!editor.draft.use_colors);
        editor.option_cursor = 1;
        editor.cycle_option(true);
        assert_eq!(editor.draft.number_format, fields::NumberFormat::Exact);
        editor.option_cursor = 3;
        editor.draft.placement = Placement::Widget;
        editor.cycle_option(true);
        assert_eq!(editor.draft.placement, Placement::Status);
        editor.option_cursor = 4;
        // The default draft has liveTokens on; one cycle turns it off.
        assert!(editor.draft.live_tokens.enabled);
        editor.cycle_option(true);
        assert!(!editor.draft.live_tokens.enabled);
        assert!(!editor.draft.live_enabled());
    }

    #[test]
    fn field_rows_show_values_or_placeholders() {
        let mut editor = editor();
        let lines = editor.render(80);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[x] session-duration"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[x] live-output-tokens"))
        );
        assert!(lines.iter().any(|line| line.contains("[?] legacy-id")));
        assert!(lines.iter().any(|line| line.contains("[ ] model")));
        // Unselected model has data, so its value is visible; session-duration
        // is selected and shows a real value (started_at is known).
        assert!(lines.iter().any(|line| line.contains("GLM-5.1")));
        // A data-less field shows the explicit placeholder.
        assert!(lines.iter().any(|line| line.contains("(git-branch)")));
    }

    #[test]
    fn dialog_is_boxed_layered_and_stays_within_the_height_budget() {
        let mut editor = editor();
        editor.on_resize(80, 20);
        let lines = editor.render(80);
        assert!(
            lines.len() <= 20,
            "frame fits the reported height: {lines:#?}"
        );
        assert_eq!(lines.len(), 20, "the body fills the remaining rows");
        assert!(lines[0].contains("rpi-statusline"), "title row: {lines:#?}");
        assert!(lines[0].contains('┌'), "boxed title: {lines:#?}");
        assert!(
            lines[1].contains("Fields")
                && lines[1].contains("Options")
                && lines[1].contains("Preview"),
            "page tabs are distinct from the list: {lines:#?}"
        );
        assert!(lines.iter().any(|line| line.contains("FIELD")));
        assert!(
            lines.iter().any(|line| line.contains("editing")),
            "status band: {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("↑/↓ move")),
            "hint band: {lines:#?}"
        );
        assert!(
            lines.last().is_some_and(|line| line.contains('┘')),
            "bottom border: {lines:#?}"
        );
        // Moving to the tail scrolls the window instead of growing the frame.
        editor.cursor = editor.rows().len().saturating_sub(1);
        let lines = editor.render(80);
        assert_eq!(lines.len(), 20, "scrolled frame keeps the same height");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("live-toolcall-tokens")),
            "the cursor row is in view: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("FIELD") && line.contains('/')),
            "range indicator: {lines:#?}"
        );
    }

    #[test]
    fn preview_shows_empty_output_guidance_and_placement_hints() {
        let mut editor = editor();
        editor.draft.set_items(&[]);
        editor.draft.placement = Placement::Widget;
        editor.page = Page::Preview;
        let lines = editor.render(80);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("built-in footer is restored"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("widget keeps the built-in footer"))
        );
        // Replace is warned against.
        editor.draft.set_items(&["model".into()]);
        editor.draft.placement = Placement::Replace;
        let lines = editor.render(80);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("replace hides the built-in footer"))
        );
        // Live fields selected with liveTokens off → pending note.
        editor.draft.set_items(&["streaming".into()]);
        editor.draft.placement = Placement::Widget;
        editor.draft.live_tokens.enabled = false;
        let lines = editor.render(80);
        assert!(lines.iter().any(|line| line.contains("liveTokens is off")));
    }

    #[test]
    fn escape_and_q_finish_the_component_without_saving() {
        let mut editor = editor();
        editor.handle_input("q");
        assert_eq!(editor.done(), Some(json!({"action": "cancelled"})));
        assert_eq!(editor.state, State::Editing);
    }

    #[test]
    fn reset_requires_a_second_keypress() {
        let mut editor = editor();
        editor.page = Page::Fields;
        editor.cursor = 0;
        editor.toggle(); // duration off
        editor.handle_input("r");
        assert!(editor.reset_armed);
        assert!(!editor.draft.has_field(FieldId::SessionDuration));
        editor.handle_input("r");
        assert!(!editor.reset_armed);
        assert!(editor.draft.has_field(FieldId::LiveOutputTokens));
        assert!(editor.draft.has_field(FieldId::LiveOutputRate));
    }

    #[test]
    fn saving_writes_the_subtree_and_marks_the_component_done() {
        let dir =
            std::env::temp_dir().join(format!("rpi-statusline-editor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            "{\"defaultModel\":\"x\",\"statusLine\":{\"type\":\"items\",\"items\":[],\"command\":\"echo old\",\"unknownKey\":1}}",
        )
        .expect("write");
        let (root, fingerprint) = settings_write::read_status_line(&path).expect("read");
        let opened = root
            .get("statusLine")
            .and_then(Value::as_object)
            .cloned()
            .unwrap();
        let mut editor = editor();
        editor.path = path.clone();
        editor.opened = opened;
        editor.fingerprint = fingerprint;
        editor.draft.set_items(&["model".into()]);
        editor.draft.placement = Placement::Widget;
        editor.save();
        assert_eq!(editor.state, State::Applied);
        assert_eq!(editor.done(), Some(json!({"action": "saved"})));
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(parsed["defaultModel"], "x");
        assert_eq!(parsed["statusLine"]["type"], "items");
        assert_eq!(parsed["statusLine"]["items"], json!(["model"]));
        assert_eq!(parsed["statusLine"]["command"], "echo old");
        assert_eq!(parsed["statusLine"]["unknownKey"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflict_keeps_the_draft_and_stays_editing() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-statusline-editor-conflict-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"statusLine\":{\"type\":\"items\",\"items\":[]}}").expect("write");
        let (root, fingerprint) = settings_write::read_status_line(&path).expect("read");
        let opened = root
            .get("statusLine")
            .and_then(Value::as_object)
            .cloned()
            .unwrap();
        // Another writer changes the subtree after open.
        std::fs::write(
            &path,
            "{\"statusLine\":{\"type\":\"items\",\"items\":[\"model\"]}}",
        )
        .expect("rewrite");
        let mut editor = editor();
        editor.path = path;
        editor.opened = opened;
        editor.fingerprint = fingerprint;
        editor.draft.set_items(&["git-branch".into()]);
        editor.save();
        assert_eq!(editor.state, State::Editing);
        assert!(editor.done().is_none());
        assert!(
            editor
                .message
                .as_deref()
                .is_some_and(|message| message.contains("changed on disk"))
        );
        // Draft preserved.
        assert!(editor.draft.has_field(FieldId::GitBranch));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_text_covers_all_three_states() {
        let none = config::SettingsSnapshot::default();
        assert!(config_text(&none).contains("not configured"));
        let items = config::SettingsSnapshot {
            status_line: Some(StatusLine::Items(ItemsConfig::default())),
            session_dir: None,
        };
        let text = config_text(&items);
        assert!(text.contains("items (Status)"));
        assert!(text.contains("live-output-tokens"));
        assert!(text.contains("live-output-rate"));
        assert!(text.contains("liveTokens: on"));
        let command = config::SettingsSnapshot {
            status_line: Some(StatusLine::Command(config::StatusLineConfig {
                command: "echo hi".into(),
                padding: 0,
                refresh_interval_secs: None,
                placement: Placement::Replace,
                timeout_ms: config::DEFAULT_TIMEOUT_MS,
                live_tokens: config::LiveTokens::default(),
            })),
            session_dir: None,
        };
        let text = config_text(&command);
        assert!(text.contains("command (Replace)"));
        assert!(text.contains("echo hi"));
    }
}
