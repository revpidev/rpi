//! Port of `packages/tui/src/components/input.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Cursor positions are in characters (upstream UTF-16 code units); every
//!   upstream cursor arithmetic (grapheme lengths, word-navigation results,
//!   string slicing) lands on the same character position in both spaces.
//! - Callbacks are `Box<dyn FnMut ... + Send>` fields instead of plain
//!   function properties.
//! - `focused` is private with the [`Focusable`] trait accessors (upstream
//!   public property, set by the TUI).

use std::borrow::Cow;

use crate::keybindings::{get_keybindings, Keybinding};
use crate::keys::decode_kitty_printable;
use crate::kill_ring::{KillRing, KillRingPushOptions};
use crate::tui::{Component, Focusable, CURSOR_MARKER};
use crate::undo_stack::UndoStack;
use crate::utils::{get_grapheme_segmenter, is_whitespace_char, slice_by_column, visible_width};
use crate::word_navigation::{find_word_backward, find_word_forward};

/// Byte offset of the `chars`-th character in `value`; `chars` == char count
/// maps to `value.len()`. Cursors are always on char boundaries.
fn char_to_byte(value: &str, chars: usize) -> usize {
    value
        .char_indices()
        .nth(chars)
        .map(|(byte, _)| byte)
        .unwrap_or(value.len())
}

/// Undo snapshot (upstream `InputState`, input.ts:11-14).
#[derive(Debug, Clone)]
struct InputState {
    value: String,
    cursor: usize,
}

/// Submit callback (upstream `(value: string) => void`).
pub type SubmitFn = Box<dyn FnMut(&str) + Send>;
/// Escape callback (upstream `() => void`).
pub type EscapeFn = Box<dyn FnMut() + Send>;

/// Last editing action, for kill-ring accumulation and undo coalescing
/// (upstream `lastAction`, input.ts:34).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Input component — single-line text input with horizontal scrolling
/// (upstream `Input`, input.ts:19).
pub struct Input {
    value: String,
    cursor: usize, // Cursor position in the value (characters)

    /// Called on submit (upstream `onSubmit`).
    pub on_submit: Option<SubmitFn>,
    /// Called on escape/cancel (upstream `onEscape`).
    pub on_escape: Option<EscapeFn>,

    /// Focusable interface — set by TUI when focus changes.
    focused: bool,

    // Bracketed paste mode buffering
    paste_buffer: String,
    is_in_paste: bool,

    // Kill ring for Emacs-style kill/yank operations
    kill_ring: KillRing,
    last_action: Option<LastAction>,

    // Undo support
    undo_stack: UndoStack<InputState>,
}

impl Input {
    pub fn new() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            on_submit: None,
            on_escape: None,
            focused: false,
            paste_buffer: String::new(),
            is_in_paste: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
        }
    }

    pub fn get_value(&self) -> &str {
        &self.value
    }

    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.cursor.min(self.value.chars().count());
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn handle_input(&mut self, data: &str) {
        // Handle bracketed paste mode
        // Start of paste: \x1b[200~
        // End of paste: \x1b[201~

        // Check if we're starting a bracketed paste
        let mut data = Cow::Borrowed(data);
        if data.contains("\x1b[200~") {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            data = Cow::Owned(data.replacen("\x1b[200~", "", 1));
        }

        // If we're in a paste, buffer the data
        if self.is_in_paste {
            self.paste_buffer.push_str(&data);

            // Check if this chunk contains the end marker
            if let Some(end_index) = self.paste_buffer.find("\x1b[201~") {
                // Extract the pasted content (owned: handle_paste reborrows
                // self mutably)
                let paste_content = self.paste_buffer[..end_index].to_string();

                // Process the complete paste
                self.handle_paste(&paste_content);

                // Reset paste state
                self.is_in_paste = false;

                // Handle any remaining input after the paste marker
                let remaining_start = end_index + 6; // 6 = length of \x1b[201~
                let remaining = self.paste_buffer[remaining_start..].to_string();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
            }
            return;
        }

        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Escape/Cancel
        if kb.matches(&data, Keybinding::SelectCancel) {
            if let Some(on_escape) = self.on_escape.as_mut() {
                on_escape();
            }
            return;
        }

        // Undo
        if kb.matches(&data, Keybinding::EditorUndo) {
            self.undo();
            return;
        }

        // Submit
        if kb.matches(&data, Keybinding::InputSubmit) || data.as_ref() == "\n" {
            if let Some(on_submit) = self.on_submit.as_mut() {
                on_submit(&self.value);
            }
            return;
        }

        // Deletion
        if kb.matches(&data, Keybinding::EditorDeleteCharBackward) {
            self.handle_backspace();
            return;
        }

        if kb.matches(&data, Keybinding::EditorDeleteCharForward) {
            self.handle_forward_delete();
            return;
        }

        if kb.matches(&data, Keybinding::EditorDeleteWordBackward) {
            self.delete_word_backwards();
            return;
        }

        if kb.matches(&data, Keybinding::EditorDeleteWordForward) {
            self.delete_word_forward();
            return;
        }

        if kb.matches(&data, Keybinding::EditorDeleteToLineStart) {
            self.delete_to_line_start();
            return;
        }

        if kb.matches(&data, Keybinding::EditorDeleteToLineEnd) {
            self.delete_to_line_end();
            return;
        }

        // Kill ring actions
        if kb.matches(&data, Keybinding::EditorYank) {
            self.yank();
            return;
        }
        if kb.matches(&data, Keybinding::EditorYankPop) {
            self.yank_pop();
            return;
        }

        // Cursor movement
        if kb.matches(&data, Keybinding::EditorCursorLeft) {
            self.last_action = None;
            if self.cursor > 0 {
                let before_cursor = &self.value[..char_to_byte(&self.value, self.cursor)];
                let last_grapheme = get_grapheme_segmenter().segment(before_cursor).next_back();
                self.cursor -= last_grapheme.map(|g| g.chars().count()).unwrap_or(1);
            }
            return;
        }

        if kb.matches(&data, Keybinding::EditorCursorRight) {
            self.last_action = None;
            if self.cursor < self.value.chars().count() {
                let after_cursor = &self.value[char_to_byte(&self.value, self.cursor)..];
                let first_grapheme = get_grapheme_segmenter().segment(after_cursor).next();
                self.cursor += first_grapheme.map(|g| g.chars().count()).unwrap_or(1);
            }
            return;
        }

        if kb.matches(&data, Keybinding::EditorCursorLineStart) {
            self.last_action = None;
            self.cursor = 0;
            return;
        }

        if kb.matches(&data, Keybinding::EditorCursorLineEnd) {
            self.last_action = None;
            self.cursor = self.value.chars().count();
            return;
        }

        if kb.matches(&data, Keybinding::EditorCursorWordLeft) {
            self.move_word_backwards();
            return;
        }

        if kb.matches(&data, Keybinding::EditorCursorWordRight) {
            self.move_word_forwards();
            return;
        }

        // Kitty CSI-u printable character (e.g. \x1b[97u for 'a').
        // Terminals with Kitty protocol flag 1 (disambiguate) send CSI-u for
        // all keys, including plain printable characters. Decode before the
        // control-char check since CSI-u sequences contain \x1b which would
        // be rejected.
        if let Some(kitty_printable) = decode_kitty_printable(&data) {
            self.insert_character(&kitty_printable.to_string());
            return;
        }

        // Regular character input - accept printable characters including
        // Unicode, but reject control characters (C0: 0x00-0x1F, DEL: 0x7F,
        // C1: 0x80-0x9F)
        let has_control_chars = data.chars().any(|ch| {
            let code = ch as u32;
            code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
        });
        if !has_control_chars {
            self.insert_character(&data);
        }
    }

    fn invalidate(&mut self) {
        // No cached state to invalidate currently
    }

    fn render(&self, width: usize) -> Vec<String> {
        // Calculate visible window
        let prompt = "> ";
        let available_width = width.saturating_sub(prompt.len());

        if available_width == 0 {
            return vec![prompt.to_string()];
        }

        let visible_text: Cow<'_, str>;
        let cursor_display: usize;
        let total_width = visible_width(&self.value);

        if total_width < available_width {
            // Everything fits (leave room for cursor at end)
            visible_text = Cow::Borrowed(&self.value);
            cursor_display = self.cursor;
        } else {
            // Need horizontal scrolling
            // Reserve one column for cursor if it's at the end
            let scroll_width = if self.cursor == self.value.chars().count() {
                available_width - 1
            } else {
                available_width
            };
            let cursor_col = visible_width(&self.value[..char_to_byte(&self.value, self.cursor)]);

            if scroll_width > 0 {
                let half_width = scroll_width / 2;
                let start_col = if cursor_col < half_width {
                    // Cursor near start
                    0
                } else if cursor_col > total_width - half_width {
                    // Cursor near end
                    total_width.saturating_sub(scroll_width)
                } else {
                    // Cursor in middle
                    cursor_col.saturating_sub(half_width)
                };

                visible_text =
                    Cow::Owned(slice_by_column(&self.value, start_col, scroll_width, true));
                let before_cursor = slice_by_column(
                    &self.value,
                    start_col,
                    cursor_col.saturating_sub(start_col),
                    true,
                );
                cursor_display = before_cursor.chars().count();
            } else {
                visible_text = Cow::Borrowed("");
                cursor_display = 0;
            }
        }

        // Build line with fake cursor
        // Insert cursor character at cursor position
        // A wide character straddling the window edge can make
        // `before_cursor` longer than `visible_text`; JS slice() clamps
        // out-of-range indices, so clamp here (the cursor then renders as a
        // space at the end of the window, like upstream).
        let cursor_display = cursor_display.min(visible_text.chars().count());
        let cursor_display_byte = char_to_byte(&visible_text, cursor_display);
        let after_cursor = &visible_text[cursor_display_byte..];
        let cursor_grapheme = get_grapheme_segmenter().segment(after_cursor).next();

        let before_cursor = &visible_text[..cursor_display_byte];
        let (at_cursor, after_cursor) = match cursor_grapheme {
            Some(grapheme) => (
                grapheme,
                &visible_text[cursor_display_byte + grapheme.len()..],
            ),
            // Character at cursor, or space if at end
            None => (" ", ""),
        };

        // Hardware cursor marker (zero-width, emitted before fake cursor for
        // IME positioning)
        let marker = if self.focused { CURSOR_MARKER } else { "" };

        // Use inverse video to show cursor
        let cursor_char = format!("\x1b[7m{at_cursor}\x1b[27m"); // ESC[7m = reverse video, ESC[27m = normal
        let text_with_cursor = format!("{before_cursor}{marker}{cursor_char}{after_cursor}");

        // Calculate visual width
        let visual_length = visible_width(&text_with_cursor);
        let padding = " ".repeat(available_width.saturating_sub(visual_length));
        let line = format!("{prompt}{text_with_cursor}{padding}");

        vec![line]
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for Input {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Input {
    fn insert_character(&mut self, char: &str) {
        // Undo coalescing: consecutive word chars coalesce into one undo unit
        if is_whitespace_char(char) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);

        let byte = char_to_byte(&self.value, self.cursor);
        self.value = format!("{}{}{}", &self.value[..byte], char, &self.value[byte..]);
        self.cursor += char.chars().count();
    }

    fn handle_backspace(&mut self) {
        self.last_action = None;
        if self.cursor > 0 {
            self.push_undo();
            let before_cursor = &self.value[..char_to_byte(&self.value, self.cursor)];
            let last_grapheme = get_grapheme_segmenter().segment(before_cursor).next_back();
            let grapheme_length = last_grapheme.map(|g| g.chars().count()).unwrap_or(1);
            let start_byte = char_to_byte(&self.value, self.cursor - grapheme_length);
            let end_byte = char_to_byte(&self.value, self.cursor);
            self.value = format!("{}{}", &self.value[..start_byte], &self.value[end_byte..]);
            self.cursor -= grapheme_length;
        }
    }

    fn handle_forward_delete(&mut self) {
        self.last_action = None;
        if self.cursor < self.value.chars().count() {
            self.push_undo();
            let after_cursor = &self.value[char_to_byte(&self.value, self.cursor)..];
            let first_grapheme = get_grapheme_segmenter().segment(after_cursor).next();
            let grapheme_length = first_grapheme.map(|g| g.chars().count()).unwrap_or(1);
            let end_byte = char_to_byte(&self.value, self.cursor + grapheme_length);
            let start_byte = char_to_byte(&self.value, self.cursor);
            self.value = format!("{}{}", &self.value[..start_byte], &self.value[end_byte..]);
        }
    }

    fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let end_byte = char_to_byte(&self.value, self.cursor);
        let deleted_text = self.value[..end_byte].to_string();
        self.kill_ring.push(
            deleted_text,
            KillRingPushOptions {
                prepend: true,
                accumulate: self.last_action == Some(LastAction::Kill),
            },
        );
        self.last_action = Some(LastAction::Kill);
        self.value = self.value[end_byte..].to_string();
        self.cursor = 0;
    }

    fn delete_to_line_end(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        self.push_undo();
        let start_byte = char_to_byte(&self.value, self.cursor);
        let deleted_text = self.value[start_byte..].to_string();
        self.kill_ring.push(
            deleted_text,
            KillRingPushOptions {
                prepend: false,
                accumulate: self.last_action == Some(LastAction::Kill),
            },
        );
        self.last_action = Some(LastAction::Kill);
        self.value = self.value[..start_byte].to_string();
    }

    fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }

        // Save lastAction before cursor movement (moveWordBackwards resets it)
        let was_kill = self.last_action == Some(LastAction::Kill);

        self.push_undo();

        let old_cursor = self.cursor;
        self.move_word_backwards();
        let delete_from = self.cursor;
        self.cursor = old_cursor;

        let delete_from_byte = char_to_byte(&self.value, delete_from);
        let cursor_byte = char_to_byte(&self.value, self.cursor);
        let deleted_text = self.value[delete_from_byte..cursor_byte].to_string();
        self.kill_ring.push(
            deleted_text,
            KillRingPushOptions {
                prepend: true,
                accumulate: was_kill,
            },
        );
        self.last_action = Some(LastAction::Kill);

        self.value = format!(
            "{}{}",
            &self.value[..delete_from_byte],
            &self.value[cursor_byte..]
        );
        self.cursor = delete_from;
    }

    fn delete_word_forward(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }

        // Save lastAction before cursor movement (moveWordForwards resets it)
        let was_kill = self.last_action == Some(LastAction::Kill);

        self.push_undo();

        let old_cursor = self.cursor;
        self.move_word_forwards();
        let delete_to = self.cursor;
        self.cursor = old_cursor;

        let cursor_byte = char_to_byte(&self.value, self.cursor);
        let delete_to_byte = char_to_byte(&self.value, delete_to);
        let deleted_text = self.value[cursor_byte..delete_to_byte].to_string();
        self.kill_ring.push(
            deleted_text,
            KillRingPushOptions {
                prepend: false,
                accumulate: was_kill,
            },
        );
        self.last_action = Some(LastAction::Kill);

        self.value = format!(
            "{}{}",
            &self.value[..cursor_byte],
            &self.value[delete_to_byte..]
        );
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };

        self.push_undo();

        let byte = char_to_byte(&self.value, self.cursor);
        self.value = format!("{}{}{}", &self.value[..byte], text, &self.value[byte..]);
        self.cursor += text.chars().count();
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }

        self.push_undo();

        // Delete the previously yanked text (still at end of ring before
        // rotation)
        let prev_text = self.kill_ring.peek().unwrap_or("").to_string();
        let start_byte = char_to_byte(&self.value, self.cursor - prev_text.chars().count());
        let end_byte = char_to_byte(&self.value, self.cursor);
        self.value = format!("{}{}", &self.value[..start_byte], &self.value[end_byte..]);
        self.cursor -= prev_text.chars().count();

        // Rotate and insert new entry
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        let byte = char_to_byte(&self.value, self.cursor);
        self.value = format!("{}{}{}", &self.value[..byte], text, &self.value[byte..]);
        self.cursor += text.chars().count();
        self.last_action = Some(LastAction::Yank);
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.value = snapshot.value;
        self.cursor = snapshot.cursor;
        self.last_action = None;
    }

    fn move_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_backward(&self.value, self.cursor, None);
    }

    fn move_word_forwards(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_forward(&self.value, self.cursor, None);
    }

    fn handle_paste(&mut self, pasted_text: &str) {
        self.last_action = None;
        self.push_undo();

        // Clean the pasted text - remove newlines and carriage returns
        let clean_text = pasted_text
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");

        // Insert at cursor position
        let byte = char_to_byte(&self.value, self.cursor);
        self.value = format!(
            "{}{}{}",
            &self.value[..byte],
            clean_text,
            &self.value[byte..]
        );
        self.cursor += clean_text.chars().count();
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `test/input.test.ts` @ pi 0.82.1 (2efa728), all 35 cases.
    //! Key sequences are sent through `handle_input` exactly like the
    //! upstream tests; matching goes through the global default keybindings.

    use super::*;
    use crate::tui::Focusable;
    use crate::utils::visible_width;

    fn input() -> Input {
        Input::new()
    }

    fn type_text(input: &mut Input, text: &str) {
        for ch in text.chars() {
            input.handle_input(&ch.to_string());
        }
    }

    #[test]
    fn submits_value_including_backslash_on_enter() {
        let mut input = input();
        let submitted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let captured = std::sync::Arc::clone(&submitted);
        input.on_submit = Some(Box::new(move |value| {
            *captured.lock().unwrap() = Some(value.to_string());
        }));

        // Type hello, then backslash, then Enter
        type_text(&mut input, "hello");
        input.handle_input("\\");
        input.handle_input("\r");

        // Input is single-line, no backslash+Enter workaround
        assert_eq!(submitted.lock().unwrap().as_deref(), Some("hello\\"));
    }

    #[test]
    fn inserts_backslash_as_regular_character() {
        let mut input = input();

        input.handle_input("\\");
        input.handle_input("x");

        assert_eq!(input.get_value(), "\\x");
    }

    #[test]
    fn render_does_not_overflow_with_wide_cjk_and_fullwidth_text() {
        let width = 93;
        let cases = [
            "가나다라마바사아자차카타파하 한글 텍스트가 터미널 너비를 초과하면 크래시가 발생합니다 이것은 재현용 테스트입니다",
            "これはテスト文章です。日本語のテキストが正しく表示されるかどうかを確認するためのサンプルテキストです。あいうえお",
            "这是一段测试文本，用于验证中文字符在终端中的显示宽度是否被正确计算，如果不正确就会导致用户界面崩溃的问题",
            "ＡＢＣＤＥＦＧＨＩＪＫＬＭＮＯＰＱＲＳＴＵＶＷＸＹＺ０１２３４５６７８９ａｂｃｄｅｆｇｈｉｊｋｌｍ",
        ];
        type CursorMove = fn(&mut Input);
        let cursor_moves: [(&str, CursorMove); 3] = [
            ("start", |_input: &mut Input| {}),
            ("middle", |input: &mut Input| {
                for _ in 0..10 {
                    input.handle_input("\x1b[C");
                }
            }),
            ("end", |input: &mut Input| input.handle_input("\x05")),
        ];

        for text in cases {
            for (label, r#move) in cursor_moves {
                let mut input = input();
                input.set_value(text);
                input.set_focused(true);
                r#move(&mut input);

                let lines = input.render(width);
                assert_eq!(lines.len(), 1, "one line expected");
                let line = &lines[0];
                assert!(
                    visible_width(line) <= width,
                    "rendered line overflowed for {text} at {label}: width {}",
                    visible_width(line)
                );
            }
        }
    }

    #[test]
    fn render_keeps_cursor_visible_when_horizontally_scrolling_wide_text() {
        let mut input = input();
        let width = 20;
        let text = "가나다라마바사아자차카타파하";
        input.set_value(text);
        input.set_focused(true);
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..5 {
            input.handle_input("\x1b[C");
        }

        let lines = input.render(width);
        assert_eq!(lines.len(), 1);
        assert!(visible_width(&lines[0]) <= width);
    }

    #[test]
    fn ctrl_w_saves_deleted_text_to_kill_ring_and_ctrl_y_yanks_it() {
        let mut input = input();

        input.set_value("foo bar baz");
        // Move cursor to end
        input.handle_input("\x05"); // Ctrl+E

        input.handle_input("\x17"); // Ctrl+W - deletes "baz"
        assert_eq!(input.get_value(), "foo bar ");

        // Move to beginning and yank
        input.handle_input("\x01"); // Ctrl+A
        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "bazfoo bar ");
    }

    #[test]
    fn ctrl_w_preserves_ascii_punctuation_boundaries() {
        let mut input = input();

        input.set_value("foo.bar");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "bar"
        assert_eq!(input.get_value(), "foo.");

        input.set_value("foo:bar");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "bar"
        assert_eq!(input.get_value(), "foo:");
    }

    #[test]
    fn ctrl_w_handles_unicode_word_boundaries() {
        let mut input = input();

        // "你好世界。你好，世界" segments as: 你好|世界|。|你好|，|世界
        input.set_value("你好世界。你好，世界");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "世界"
        assert_eq!(input.get_value(), "你好世界。你好，");
        input.handle_input("\x17"); // Ctrl+W - deletes "，"
        assert_eq!(input.get_value(), "你好世界。你好");
        input.handle_input("\x17"); // Ctrl+W - deletes "你好"
        assert_eq!(input.get_value(), "你好世界。");
        input.handle_input("\x17"); // Ctrl+W - deletes "。"
        assert_eq!(input.get_value(), "你好世界");
        input.handle_input("\x17"); // Ctrl+W - deletes "世界"
        assert_eq!(input.get_value(), "你好");
        input.handle_input("\x17"); // Ctrl+W - deletes "你好"
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn ctrl_u_saves_deleted_text_to_kill_ring() {
        let mut input = input();

        input.set_value("hello world");
        // Move cursor to after "hello "
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            input.handle_input("\x1b[C");
        }

        input.handle_input("\x15"); // Ctrl+U - deletes "hello "
        assert_eq!(input.get_value(), "world");

        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn ctrl_k_saves_deleted_text_to_kill_ring() {
        let mut input = input();

        input.set_value("hello world");
        input.handle_input("\x01"); // Ctrl+A
        input.handle_input("\x0b"); // Ctrl+K - deletes "hello world"

        assert_eq!(input.get_value(), "");

        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn ctrl_y_does_nothing_when_kill_ring_is_empty() {
        let mut input = input();

        input.set_value("test");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "test");
    }

    #[test]
    fn alt_y_cycles_through_kill_ring_after_ctrl_y() {
        let mut input = input();

        // Create kill ring with multiple entries
        input.set_value("first");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "first"
        input.set_value("second");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "second"
        input.set_value("third");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "third"

        assert_eq!(input.get_value(), "");

        input.handle_input("\x19"); // Ctrl+Y - yanks "third"
        assert_eq!(input.get_value(), "third");

        input.handle_input("\x1by"); // Alt+Y - cycles to "second"
        assert_eq!(input.get_value(), "second");

        input.handle_input("\x1by"); // Alt+Y - cycles to "first"
        assert_eq!(input.get_value(), "first");

        input.handle_input("\x1by"); // Alt+Y - cycles back to "third"
        assert_eq!(input.get_value(), "third");
    }

    #[test]
    fn alt_y_does_nothing_if_not_preceded_by_yank() {
        let mut input = input();

        input.set_value("test");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "test"
        input.set_value("other");
        input.handle_input("\x05"); // Ctrl+E

        // Type something to break the yank chain
        input.handle_input("x");
        assert_eq!(input.get_value(), "otherx");

        input.handle_input("\x1by"); // Alt+Y - should do nothing
        assert_eq!(input.get_value(), "otherx");
    }

    #[test]
    fn alt_y_does_nothing_if_kill_ring_has_one_entry() {
        let mut input = input();

        input.set_value("only");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "only"

        input.handle_input("\x19"); // Ctrl+Y - yanks "only"
        assert_eq!(input.get_value(), "only");

        input.handle_input("\x1by"); // Alt+Y - should do nothing
        assert_eq!(input.get_value(), "only");
    }

    #[test]
    fn consecutive_ctrl_w_accumulates_into_one_kill_ring_entry() {
        let mut input = input();

        input.set_value("one two three");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "three"
        input.handle_input("\x17"); // Ctrl+W - deletes "two "
        input.handle_input("\x17"); // Ctrl+W - deletes "one "

        assert_eq!(input.get_value(), "");

        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "one two three");
    }

    #[test]
    fn non_delete_actions_break_kill_accumulation() {
        let mut input = input();

        input.set_value("foo bar baz");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "baz"
        assert_eq!(input.get_value(), "foo bar ");

        input.handle_input("x"); // Typing breaks accumulation
        assert_eq!(input.get_value(), "foo bar x");

        input.handle_input("\x17"); // Ctrl+W - deletes "x" (separate entry)
        assert_eq!(input.get_value(), "foo bar ");

        input.handle_input("\x19"); // Ctrl+Y - most recent is "x"
        assert_eq!(input.get_value(), "foo bar x");

        input.handle_input("\x1by"); // Alt+Y - cycle to "baz"
        assert_eq!(input.get_value(), "foo bar baz");
    }

    #[test]
    fn non_yank_actions_break_alt_y_chain() {
        let mut input = input();

        input.set_value("first");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W
        input.set_value("second");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W
        input.set_value("");

        input.handle_input("\x19"); // Ctrl+Y - yanks "second"
        assert_eq!(input.get_value(), "second");

        input.handle_input("x"); // Breaks yank chain
        assert_eq!(input.get_value(), "secondx");

        input.handle_input("\x1by"); // Alt+Y - should do nothing
        assert_eq!(input.get_value(), "secondx");
    }

    #[test]
    fn kill_ring_rotation_persists_after_cycling() {
        let mut input = input();

        input.set_value("first");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // deletes "first"
        input.set_value("second");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // deletes "second"
        input.set_value("third");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // deletes "third"
        input.set_value("");

        input.handle_input("\x19"); // Ctrl+Y - yanks "third"
        input.handle_input("\x1by"); // Alt+Y - cycles to "second"
        assert_eq!(input.get_value(), "second");

        // Break chain and start fresh
        input.handle_input("x");
        input.set_value("");

        // New yank should get "second" (now at end after rotation)
        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "second");
    }

    #[test]
    fn backward_deletions_prepend_forward_deletions_append_during_accumulation() {
        let mut input = input();

        input.set_value("prefix|suffix");
        // Position cursor at "|"
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            input.handle_input("\x1b[C"); // Move right 6
        }

        input.handle_input("\x0b"); // Ctrl+K - deletes "|suffix" (forward)
        assert_eq!(input.get_value(), "prefix");

        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "prefix|suffix");
    }

    #[test]
    fn alt_d_deletes_word_forward_and_saves_to_kill_ring() {
        let mut input = input();

        input.set_value("hello world test");
        input.handle_input("\x01"); // Ctrl+A

        input.handle_input("\x1bd"); // Alt+D - deletes "hello"
        assert_eq!(input.get_value(), " world test");

        input.handle_input("\x1bd"); // Alt+D - deletes " world"
        assert_eq!(input.get_value(), " test");

        // Yank should get accumulated text
        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "hello world test");
    }

    #[test]
    fn alt_d_preserves_ascii_punctuation_boundaries() {
        let mut input = input();

        input.set_value("foo.bar baz");
        input.handle_input("\x01"); // Ctrl+A
        input.handle_input("\x1bd"); // Alt+D - deletes "foo"
        assert_eq!(input.get_value(), ".bar baz");
        input.handle_input("\x1bd"); // Alt+D - deletes "."
        assert_eq!(input.get_value(), "bar baz");
        input.handle_input("\x1bd"); // Alt+D - deletes "bar"
        assert_eq!(input.get_value(), " baz");
    }

    #[test]
    fn alt_d_handles_unicode_word_boundaries() {
        let mut input = input();

        // "你好世界。你好，世界" segments as: 你好|世界|。|你好|，|世界
        input.set_value("你好世界。你好，世界");
        input.handle_input("\x01"); // Ctrl+A
        input.handle_input("\x1bd"); // Alt+D - deletes "你好"
        assert_eq!(input.get_value(), "世界。你好，世界");
        input.handle_input("\x1bd"); // Alt+D - deletes "世界"
        assert_eq!(input.get_value(), "。你好，世界");
        input.handle_input("\x1bd"); // Alt+D - deletes "。"
        assert_eq!(input.get_value(), "你好，世界");
        input.handle_input("\x1bd"); // Alt+D - deletes "你好"
        assert_eq!(input.get_value(), "，世界");
        input.handle_input("\x1bd"); // Alt+D - deletes "，"
        assert_eq!(input.get_value(), "世界");
        input.handle_input("\x1bd"); // Alt+D - deletes "世界"
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn handles_yank_in_middle_of_text() {
        let mut input = input();

        input.set_value("word");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "word"
        input.set_value("hello world");
        // Move to middle (after "hello ")
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            input.handle_input("\x1b[C");
        }

        input.handle_input("\x19"); // Ctrl+Y
        assert_eq!(input.get_value(), "hello wordworld");
    }

    #[test]
    fn handles_yank_pop_in_middle_of_text() {
        let mut input = input();

        // Create two kill ring entries
        input.set_value("FIRST");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "FIRST"
        input.set_value("SECOND");
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("\x17"); // Ctrl+W - deletes "SECOND"

        // Set up "hello world" and position cursor after "hello "
        input.set_value("hello world");
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            input.handle_input("\x1b[C");
        }

        input.handle_input("\x19"); // Ctrl+Y - yanks "SECOND"
        assert_eq!(input.get_value(), "hello SECONDworld");

        input.handle_input("\x1by"); // Alt+Y - replaces with "FIRST"
        assert_eq!(input.get_value(), "hello FIRSTworld");
    }

    #[test]
    fn undo_does_nothing_when_undo_stack_is_empty() {
        let mut input = input();

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn undo_coalesces_consecutive_word_characters_into_one_undo_unit() {
        let mut input = input();

        type_text(&mut input, "hello world");
        assert_eq!(input.get_value(), "hello world");

        // Undo removes " world"
        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello");

        // Undo removes "hello"
        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn undo_undoes_spaces_one_at_a_time() {
        let mut input = input();

        type_text(&mut input, "hello  ");
        assert_eq!(input.get_value(), "hello  ");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo) - removes second " "
        assert_eq!(input.get_value(), "hello ");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo) - removes first " "
        assert_eq!(input.get_value(), "hello");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo) - removes "hello"
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn undo_undoes_backspace() {
        let mut input = input();

        type_text(&mut input, "hello");
        input.handle_input("\x7f"); // Backspace
        assert_eq!(input.get_value(), "hell");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello");
    }

    #[test]
    fn undo_undoes_forward_delete() {
        let mut input = input();

        type_text(&mut input, "hello");
        input.handle_input("\x01"); // Ctrl+A - go to start
        input.handle_input("\x1b[C"); // Right arrow
        input.handle_input("\x1b[3~"); // Delete key
        assert_eq!(input.get_value(), "hllo");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello");
    }

    #[test]
    fn undo_undoes_ctrl_w() {
        let mut input = input();

        type_text(&mut input, "hello world");
        assert_eq!(input.get_value(), "hello world");

        input.handle_input("\x17"); // Ctrl+W
        assert_eq!(input.get_value(), "hello ");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn undo_undoes_ctrl_k() {
        let mut input = input();

        type_text(&mut input, "hello world");
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            input.handle_input("\x1b[C");
        }

        input.handle_input("\x0b"); // Ctrl+K
        assert_eq!(input.get_value(), "hello ");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn undo_undoes_ctrl_u() {
        let mut input = input();

        type_text(&mut input, "hello world");
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            input.handle_input("\x1b[C");
        }

        input.handle_input("\x15"); // Ctrl+U
        assert_eq!(input.get_value(), "world");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn undo_undoes_yank() {
        let mut input = input();

        type_text(&mut input, "hello ");
        input.handle_input("\x17"); // Ctrl+W - delete "hello "
        input.handle_input("\x19"); // Ctrl+Y - yank
        assert_eq!(input.get_value(), "hello ");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn undo_undoes_paste_atomically() {
        let mut input = input();

        input.set_value("hello world");
        input.handle_input("\x01"); // Ctrl+A
        for _ in 0..5 {
            input.handle_input("\x1b[C");
        }

        // Simulate bracketed paste
        input.handle_input("\x1b[200~beep boop\x1b[201~");
        assert_eq!(input.get_value(), "hellobeep boop world");

        // Single undo should restore entire pre-paste state
        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn undo_undoes_alt_d() {
        let mut input = input();

        input.set_value("hello world");
        input.handle_input("\x01"); // Ctrl+A

        input.handle_input("\x1bd"); // Alt+D - deletes "hello"
        assert_eq!(input.get_value(), " world");

        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn undo_cursor_movement_starts_new_undo_unit() {
        let mut input = input();

        type_text(&mut input, "abc");
        input.handle_input("\x01"); // Ctrl+A - movement breaks coalescing
        input.handle_input("\x05"); // Ctrl+E
        input.handle_input("d");
        input.handle_input("e");
        assert_eq!(input.get_value(), "abcde");

        // Undo removes "de" (typed after movement)
        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "abc");

        // Undo removes "abc"
        input.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(input.get_value(), "");
    }
}
