//! Test virtual terminal shared by the TUI test suites (port of
//! `test/virtual-terminal.ts` @ 4181f66; extended for T31, see below).
//!
//! Upstream uses `@xterm/headless`; here a minimal screen emulator covers
//! exactly the sequences the TUI emits (CSI A/B/G/H/J/K, SGR with per-cell
//! italic tracking for the style-leak assertions, CR/LF with scrolling,
//! private modes, OSC/APC/DCS skipping). `translateToString(true)` parity:
//! cells never written are dropped from the end of a line; explicitly
//! written spaces count.
//!
//! T31 additions (semantic extensions; the T28 main-screen suite only uses
//! the upstream-parity subset):
//! - DECAWM (`\x1b[?7h`/`l`): with autowrap disabled, writes at the right
//!   margin stay on the last column and overwrite it instead of wrapping.
//!   `tui-alt-screen.ts` enters the alt screen with autowrap disabled so
//!   lines of exactly terminal width do not double-space.
//! - Alternate screen buffer (`\x1b[?1049h`/`l`): saves/restores the main
//!   screen and cursor; the alt screen starts blank. `get_viewport` reflects
//!   the active buffer and `get_main_screen_viewport` reads the saved main
//!   screen while the alt screen is active.
//! - Mouse modes (`?1000`/`?1002`/`?1003`/`?1004`/`?1006` h/l) fall into the
//!   unknown-private-mode catch-all: silently ignored, no screen effect.
//! - `RecordingTerminal` records the ordered `start`/`write`/`stop` event
//!   stream (upstream `RecordingTerminal` in tui-alt-screen.test.ts) for
//!   lifecycle-ordering assertions.
//! - Input-injection helpers (`sgr_mouse`/`sgr_wheel`/`x10_wheel`) and OSC 52
//!   helpers (`osc52_sequence`/`osc52_payloads`) replace the upstream test
//!   literals and `Buffer.from(_, "base64")` round-trips.
//!
//! The `settle`/`render_and_flush`/`send_input` drive helpers (upstream
//! `waitForRender` = nextTick + 20ms settle + flush) are generic over
//! `TestTui`, the tick interface shared by tick-driven TUI test doubles; each
//! suite implements it for its TUI type (delegating to the inherent
//! `tick`/`has_pending_work` methods).

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;
use unicode_width::UnicodeWidthChar;

use crate::terminal::{InputHandler, ResizeHandler, Terminal};
use crate::tui::{lock_shared, Tui};

/// Serializes tests that mutate process env or module globals (capabilities
/// cache, cell dimensions); cargo runs tests in one binary with parallel
/// threads. Mirrors terminal_image.rs's TEST_STATE_LOCK.
static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

/// Guard held by tests that mutate process env or module globals.
pub(crate) fn state_lock() -> MutexGuard<'static, ()> {
    TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `withEnv` (tui-render.test.ts:43-65): set/unset an env var, restore on drop.
pub(crate) struct EnvGuard {
    key: &'static str,
    saved: Option<String>,
}

impl EnvGuard {
    pub(crate) fn set(key: &'static str, value: Option<&str>) -> EnvGuard {
        let saved = std::env::var(key).ok();
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        EnvGuard { key, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

// ---------------------------------------------------------------------------
// VirtualTerminal (port of test/virtual-terminal.ts)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct VtCell {
    text: String,
    italic: bool,
    /// Second cell of a wide (CJK) character: emits nothing.
    continuation: bool,
}

/// `None` = never written / cleared (xterm null cell).
type VtLine = Vec<Option<VtCell>>;

/// Main-screen state saved by `\x1b[?1049h` (xterm alternate screen buffer).
struct SavedScreen {
    lines: Vec<VtLine>,
    cursor_row: usize,
    cursor_col: usize,
}

struct VtState {
    cols: usize,
    rows: usize,
    /// Scrollback + screen; the screen is the last `rows` entries.
    /// Invariant: `lines.len() >= rows`.
    lines: Vec<VtLine>,
    /// Screen-relative cursor row.
    cursor_row: usize,
    /// Cursor column; may equal `cols` (pending wrap, like xterm).
    cursor_col: usize,
    italic: bool,
    cursor_hidden: bool,
    /// DECAWM (`\x1b[?7h`/`l`); enabled by default.
    autowrap: bool,
    /// Saved main screen while the alternate screen buffer is active.
    main: Option<SavedScreen>,
    /// Every `write` recorded (upstream `LoggingVirtualTerminal`).
    writes: Vec<String>,
    input_handler: Option<InputHandler>,
    resize_handler: Option<ResizeHandler>,
}

/// Clonable handle so tests keep access after the TUI takes ownership.
#[derive(Clone)]
pub(crate) struct VirtualTerminal {
    state: Arc<Mutex<VtState>>,
}

impl Default for VirtualTerminal {
    fn default() -> Self {
        VirtualTerminal::new(80, 24)
    }
}

impl VirtualTerminal {
    pub(crate) fn new(columns: usize, rows: usize) -> Self {
        VirtualTerminal {
            state: Arc::new(Mutex::new(VtState {
                cols: columns,
                rows,
                lines: vec![vec![None; columns]; rows],
                cursor_row: 0,
                cursor_col: 0,
                italic: false,
                cursor_hidden: false,
                autowrap: true,
                main: None,
                writes: Vec::new(),
                input_handler: None,
                resize_handler: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, VtState> {
        lock_shared(&self.state)
    }

    /// Simulate keyboard input (upstream `sendInput`).
    pub(crate) fn send_input(&self, data: &str) {
        let handler = self.lock().input_handler.take();
        if let Some(mut handler) = handler {
            handler(data);
            let mut state = self.lock();
            if state.input_handler.is_none() {
                state.input_handler = Some(handler);
            }
        }
    }

    /// Resize the terminal (upstream `resize`).
    pub(crate) fn resize(&self, columns: usize, rows: usize) {
        {
            let mut state = self.lock();
            state.cols = columns;
            state.rows = rows;
            for line in &mut state.lines {
                line.resize(columns, None);
            }
            while state.lines.len() < rows {
                state.lines.push(vec![None; columns]);
            }
            if let Some(main) = &mut state.main {
                for line in &mut main.lines {
                    line.resize(columns, None);
                }
                while main.lines.len() < rows {
                    main.lines.push(vec![None; columns]);
                }
            }
            state.cursor_row = state.cursor_row.min(rows - 1);
            state.cursor_col = state.cursor_col.min(columns);
        }
        let handler = self.lock().resize_handler.take();
        if let Some(mut handler) = handler {
            handler();
            let mut state = self.lock();
            if state.resize_handler.is_none() {
                state.resize_handler = Some(handler);
            }
        }
    }

    /// The visible viewport of the active buffer (upstream `getViewport`).
    pub(crate) fn get_viewport(&self) -> Vec<String> {
        let state = self.lock();
        let screen_top = state.lines.len() - state.rows;
        state.lines[screen_top..]
            .iter()
            .map(translate_line)
            .collect()
    }

    /// The visible viewport of the main screen: the saved buffer while the
    /// alt screen is active, the active buffer otherwise (Rust addition for
    /// the exit-and-redraw assertions in tui-alt-screen.test.ts).
    pub(crate) fn get_main_screen_viewport(&self) -> Vec<String> {
        let state = self.lock();
        let lines = match &state.main {
            Some(main) => &main.lines,
            None => &state.lines,
        };
        let screen_top = lines.len() - state.rows;
        lines[screen_top..].iter().map(translate_line).collect()
    }

    /// Whether the alternate screen buffer is active (`?1049h` without the
    /// matching `?1049l`).
    pub(crate) fn alt_screen_active(&self) -> bool {
        self.lock().main.is_some()
    }

    /// The entire scroll buffer of the active buffer (upstream
    /// `getScrollBuffer`).
    pub(crate) fn get_scroll_buffer(&self) -> Vec<String> {
        self.lock().lines.iter().map(translate_line).collect()
    }

    /// Cursor position, screen-relative (upstream `getCursorPosition`).
    pub(crate) fn get_cursor_position(&self) -> (usize, usize) {
        let state = self.lock();
        (state.cursor_col, state.cursor_row)
    }

    /// Screen row count for test assertions (the usize twin of
    /// `Terminal::rows`, which returns `u16` for the trait API).
    pub(crate) fn rows(&self) -> usize {
        self.lock().rows
    }

    /// Screen column count for test assertions (the usize twin of
    /// `Terminal::columns`).
    pub(crate) fn columns(&self) -> usize {
        self.lock().cols
    }

    /// Whether the cursor is hidden (tracks `?25l` / `?25h`).
    pub(crate) fn cursor_hidden(&self) -> bool {
        self.lock().cursor_hidden
    }

    /// xterm `cell.isItalic()` for the style-leak assertions.
    pub(crate) fn cell_italic(&self, row: usize, col: usize) -> bool {
        let state = self.lock();
        let screen_top = state.lines.len() - state.rows;
        state.lines[screen_top + row]
            .get(col)
            .and_then(|cell| cell.as_ref())
            .is_some_and(|cell| cell.italic)
    }

    /// Concatenated recorded writes (upstream `getWrites`).
    pub(crate) fn get_writes(&self) -> String {
        self.lock().writes.concat()
    }

    pub(crate) fn clear_writes(&self) {
        self.lock().writes.clear();
    }
}

/// xterm `line.translateToString(true)`: trailing never-written cells are
/// dropped; written spaces and wide-char continuations are handled.
fn translate_line(line: &VtLine) -> String {
    let last_written = line.iter().rposition(Option::is_some);
    let Some(last) = last_written else {
        return String::new();
    };
    let mut out = String::new();
    for cell in &line[..=last] {
        match cell {
            None => out.push(' '),
            Some(cell) if cell.continuation => {}
            Some(cell) => out.push_str(&cell.text),
        }
    }
    out
}

impl VtState {
    fn blank_line(cols: usize) -> VtLine {
        vec![None; cols]
    }

    fn screen_top(&self) -> usize {
        self.lines.len() - self.rows
    }

    fn abs_row(&self) -> usize {
        self.screen_top() + self.cursor_row
    }

    fn line_feed(&mut self) {
        if self.cursor_row == self.rows - 1 {
            self.lines.push(Self::blank_line(self.cols));
        } else {
            self.cursor_row += 1;
        }
    }

    fn put_char(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width == 0 {
            // Combining char: merge into the previous cell's text.
            if self.cursor_col > 0 {
                let abs = self.abs_row();
                if let Some(Some(cell)) = self.lines[abs].get_mut(self.cursor_col - 1) {
                    if !cell.continuation {
                        cell.text.push(ch);
                    }
                }
            }
            return;
        }
        if self.cursor_col >= self.cols {
            if self.autowrap {
                // Pending wrap: move to the next line first (xterm wraparound).
                self.cursor_col = 0;
                self.line_feed();
            } else {
                // DECAWM off: clamp back onto the last column (see below).
                self.cursor_col = self.cols - 1;
            }
        }
        let abs = self.abs_row();
        let col = self.cursor_col;
        let italic = self.italic;
        self.lines[abs][col] = Some(VtCell {
            text: ch.to_string(),
            italic,
            continuation: false,
        });
        if width == 2 && col + 1 < self.cols {
            self.lines[abs][col + 1] = Some(VtCell {
                text: String::new(),
                italic,
                continuation: true,
            });
        }
        self.cursor_col += width;
        if !self.autowrap && self.cursor_col >= self.cols {
            // With autowrap disabled the cursor stays on the right margin
            // instead of entering the pending-wrap state, so the next write
            // overwrites the last column (xterm DECAWM reset).
            self.cursor_col = self.cols - 1;
        }
    }

    fn clear_line_range(&mut self, from: usize, to: usize) {
        let abs = self.abs_row();
        for col in from..=to.min(self.cols - 1) {
            if col < self.cols {
                self.lines[abs][col] = None;
            }
        }
    }

    fn toggle_alt_screen(&mut self, enter: bool) {
        if enter {
            if self.main.is_some() {
                return; // Already in the alt screen: idempotent.
            }
            self.main = Some(SavedScreen {
                lines: std::mem::take(&mut self.lines),
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
            });
            // The alt screen starts blank (xterm clears it on entry).
            self.lines = vec![Self::blank_line(self.cols); self.rows];
            self.cursor_row = 0;
            self.cursor_col = 0;
        } else if let Some(saved) = self.main.take() {
            self.lines = saved.lines;
            self.cursor_row = saved.cursor_row.min(self.rows - 1);
            self.cursor_col = saved.cursor_col.min(self.cols);
        }
    }

    fn handle_csi(&mut self, params: &str, final_byte: char) {
        let first_param = || {
            params
                .split(';')
                .next()
                .and_then(|p| p.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1)
        };
        match final_byte {
            'A' => self.cursor_row = self.cursor_row.saturating_sub(first_param()),
            'B' => self.cursor_row = (self.cursor_row + first_param()).min(self.rows - 1),
            'C' => self.cursor_col = (self.cursor_col + first_param()).min(self.cols - 1),
            'D' => self.cursor_col = self.cursor_col.saturating_sub(first_param()),
            'G' => {
                self.cursor_col = first_param().saturating_sub(1).min(self.cols - 1);
            }
            'H' => {
                // CUP: absolute 1-based row;col, clamped to the screen
                // (xterm behavior; the T31 alt-screen renderer positions
                // every row with `\x1b[{row+1};1H`).
                let mut parts = params.split(';');
                let row = parts
                    .next()
                    .and_then(|p| p.parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(1);
                let col = parts
                    .next()
                    .and_then(|p| p.parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(1);
                self.cursor_row = (row - 1).min(self.rows - 1);
                self.cursor_col = (col - 1).min(self.cols - 1);
            }
            'J' => match params {
                "2" => {
                    let top = self.screen_top();
                    let blank = Self::blank_line(self.cols);
                    for line in &mut self.lines[top..] {
                        *line = blank.clone();
                    }
                }
                "3" => {
                    // Clear scrollback: keep only the screen lines.
                    let top = self.screen_top();
                    self.lines.drain(..top);
                }
                _ => {
                    // Clear from cursor to end of screen.
                    let abs = self.abs_row();
                    for col in self.cursor_col..self.cols {
                        self.lines[abs][col] = None;
                    }
                    let blank = Self::blank_line(self.cols);
                    for row in abs + 1..self.lines.len() {
                        self.lines[row] = blank.clone();
                    }
                }
            },
            'K' => match params {
                "2" => {
                    let abs = self.abs_row();
                    self.lines[abs] = Self::blank_line(self.cols);
                }
                "1" => {
                    let col = self.cursor_col;
                    self.clear_line_range(0, col);
                }
                _ => {
                    let col = self.cursor_col;
                    self.clear_line_range(col, self.cols - 1);
                }
            },
            'm' => {
                for param in params.split(';') {
                    match param {
                        "" | "0" => self.italic = false,
                        "3" => self.italic = true,
                        "23" => self.italic = false,
                        _ => {}
                    }
                }
            }
            'h' | 'l' if params == "?25" => {
                self.cursor_hidden = final_byte == 'l';
            }
            'h' | 'l' if params == "?7" => {
                self.autowrap = final_byte == 'h';
            }
            'h' | 'l' if params == "?1049" => {
                self.toggle_alt_screen(final_byte == 'h');
            }
            // Unknown private modes (mouse `?1000`/`?1002`/`?1003`/`?1004`/
            // `?1006`, synchronized output `?2026`, ...) are ignored like
            // xterm's non-effect modes.
            _ => {}
        }
    }

    /// Feed output into the emulator (and record it).
    fn write_raw(&mut self, data: &str) {
        self.writes.push(data.to_string());
        let chars: Vec<char> = data.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\x1b' => match chars.get(i + 1) {
                    Some('[') => {
                        let mut j = i + 2;
                        let mut params = String::new();
                        while j < chars.len() {
                            let ch = chars[j];
                            if ('\x40'..='\x7e').contains(&ch) {
                                break;
                            }
                            params.push(ch);
                            j += 1;
                        }
                        if let Some(&final_byte) = chars.get(j) {
                            self.handle_csi(&params, final_byte);
                            j += 1;
                        }
                        i = j;
                    }
                    // OSC / APC / DCS / SOS / PM: skip to BEL or ST.
                    Some(']') | Some('_') | Some('P') | Some('X') | Some('^') => {
                        let mut j = i + 2;
                        while j < chars.len() {
                            if chars[j] == '\x07' {
                                j += 1;
                                break;
                            }
                            if chars[j] == '\x1b' && chars.get(j + 1) == Some(&'\\') {
                                j += 2;
                                break;
                            }
                            j += 1;
                        }
                        i = j;
                    }
                    Some(_) => i += 2,
                    None => i += 1,
                },
                '\r' => {
                    self.cursor_col = 0;
                    i += 1;
                }
                '\n' => {
                    self.line_feed();
                    i += 1;
                }
                '\t' => {
                    let next_tab_stop = (self.cursor_col / 8 + 1) * 8;
                    while self.cursor_col < next_tab_stop.min(self.cols) {
                        self.put_char(' ');
                    }
                    i += 1;
                }
                ch if (ch as u32) < 0x20 => i += 1,
                ch => {
                    self.put_char(ch);
                    i += 1;
                }
            }
        }
    }
}

impl Terminal for VirtualTerminal {
    fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
        let mut state = self.lock();
        state.input_handler = Some(on_input);
        state.resize_handler = Some(on_resize);
        // Enable bracketed paste mode for consistency with ProcessTerminal.
        state.write_raw("\x1b[?2004h");
    }

    fn stop(&mut self) {
        let mut state = self.lock();
        state.write_raw("\x1b[?2004l");
        state.input_handler = None;
        state.resize_handler = None;
    }

    fn drain_input(
        &mut self,
        _max_ms: Option<u64>,
        _idle_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // No-op for the virtual terminal — no stdin to drain.
        Box::pin(async {})
    }

    fn write(&mut self, data: &str) {
        self.lock().write_raw(data);
    }

    fn columns(&self) -> u16 {
        self.lock().cols as u16
    }

    fn rows(&self) -> u16 {
        self.lock().rows as u16
    }

    fn kitty_protocol_active(&self) -> bool {
        // The virtual terminal always reports Kitty protocol as active.
        true
    }

    fn move_by(&mut self, lines: i32) {
        if lines > 0 {
            self.write(&format!("\x1b[{lines}B"));
        } else if lines < 0 {
            self.write(&format!("\x1b[{}A", -lines));
        }
    }

    fn hide_cursor(&mut self) {
        self.write("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.write("\x1b[?25h");
    }

    fn clear_line(&mut self) {
        self.write("\x1b[K");
    }

    fn clear_from_cursor(&mut self) {
        self.write("\x1b[J");
    }

    fn clear_screen(&mut self) {
        self.write("\x1b[2J\x1b[H");
    }

    fn set_title(&mut self, title: &str) {
        self.write(&format!("\x1b]0;{title}\x07"));
    }

    fn set_progress(&mut self, _active: bool) {}
}

// ---------------------------------------------------------------------------
// Drive helpers (upstream `waitForRender` / `renderAndFlush` / `sendInput`)
// ---------------------------------------------------------------------------

/// Tick interface shared by tick-driven TUI test doubles. Each suite
/// implements it for its TUI type, delegating to the inherent
/// `tick`/`has_pending_work` methods (see the TuiMainScreen test module;
/// TuiAltScreen implements it with T31).
pub(crate) trait TestTui {
    fn tick(&self, now: Instant);
    fn has_pending_work(&self) -> bool;
}

/// Upstream `waitForRender` (virtual-terminal.ts:212-217): drive the TUI
/// until no work is pending, advancing synthetic time in 20ms steps (the
/// upstream settle delay). Returns the final synthetic instant so throttle
/// tests can schedule relative to it.
pub(crate) fn settle(tui: &impl TestTui) -> Instant {
    let mut now = Instant::now();
    loop {
        tui.tick(now);
        if !tui.has_pending_work() {
            return now;
        }
        now += Duration::from_millis(20);
    }
}

/// Upstream `renderAndFlush`: forced render + settle.
pub(crate) fn render_and_flush(tui: &(impl TestTui + Tui)) {
    tui.request_render(true);
    settle(tui);
}

/// `sendInput` + processing tick: input is queued to the TUI's inbox and
/// drained by the tick.
pub(crate) fn send_input(terminal: &VirtualTerminal, tui: &impl TestTui, data: &str) {
    terminal.send_input(data);
    tui.tick(Instant::now());
}

// ---------------------------------------------------------------------------
// RecordingTerminal (upstream `RecordingTerminal` in tui-alt-screen.test.ts)
// ---------------------------------------------------------------------------

/// Ordered lifecycle event recorded by [`RecordingTerminal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VtEvent {
    Start,
    Stop,
    Write(String),
}

/// Virtual terminal that records the ordered `start`/`write`/`stop` event
/// stream for lifecycle-ordering assertions (1049h before start, 1006l
/// before stop, 1049l after stop). Screen state lives in the wrapped
/// [`VirtualTerminal`]; `move_by`/`hide_cursor`/... go straight to the inner
/// terminal like upstream's `this.xterm.write` calls and are not recorded.
/// The event log is shared between clones so the test keeps reading events
/// after the TUI takes ownership of its own clone (upstream keeps the live
/// `RecordingTerminal` object).
#[derive(Clone)]
pub(crate) struct RecordingTerminal {
    inner: VirtualTerminal,
    events: Arc<Mutex<Vec<VtEvent>>>,
}

impl Default for RecordingTerminal {
    fn default() -> Self {
        RecordingTerminal::new(80, 24)
    }
}

impl RecordingTerminal {
    pub(crate) fn new(columns: usize, rows: usize) -> Self {
        RecordingTerminal {
            inner: VirtualTerminal::new(columns, rows),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The ordered event stream (upstream `RecordingTerminal.events`).
    pub(crate) fn events(&self) -> Vec<VtEvent> {
        lock_shared(&self.events).clone()
    }

    /// Concatenated recorded write payloads (upstream
    /// `events.filter(write).map(data).join("")`).
    pub(crate) fn writes(&self) -> String {
        lock_shared(&self.events)
            .iter()
            .filter_map(|event| match event {
                VtEvent::Write(data) => Some(data.as_str()),
                _ => None,
            })
            .collect()
    }
}

impl Deref for RecordingTerminal {
    type Target = VirtualTerminal;

    fn deref(&self) -> &VirtualTerminal {
        &self.inner
    }
}

impl Terminal for RecordingTerminal {
    fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
        lock_shared(&self.events).push(VtEvent::Start);
        self.inner.start(on_input, on_resize);
    }

    fn stop(&mut self) {
        lock_shared(&self.events).push(VtEvent::Stop);
        self.inner.stop();
    }

    fn drain_input(
        &mut self,
        max_ms: Option<u64>,
        idle_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.inner.drain_input(max_ms, idle_ms)
    }

    fn write(&mut self, data: &str) {
        lock_shared(&self.events).push(VtEvent::Write(data.to_string()));
        self.inner.write(data);
    }

    fn columns(&self) -> u16 {
        // Explicit trait call: the inherent `columns` on `VirtualTerminal`
        // returns the usize screen dimension for test assertions.
        Terminal::columns(&self.inner)
    }

    fn rows(&self) -> u16 {
        Terminal::rows(&self.inner)
    }

    fn kitty_protocol_active(&self) -> bool {
        self.inner.kitty_protocol_active()
    }

    fn move_by(&mut self, lines: i32) {
        self.inner.move_by(lines);
    }

    fn hide_cursor(&mut self) {
        self.inner.hide_cursor();
    }

    fn show_cursor(&mut self) {
        self.inner.show_cursor();
    }

    fn clear_line(&mut self) {
        self.inner.clear_line();
    }

    fn clear_from_cursor(&mut self) {
        self.inner.clear_from_cursor();
    }

    fn clear_screen(&mut self) {
        self.inner.clear_screen();
    }

    fn set_title(&mut self, title: &str) {
        self.inner.set_title(title);
    }

    fn set_progress(&mut self, active: bool) {
        self.inner.set_progress(active);
    }
}

// ---------------------------------------------------------------------------
// Input-injection and OSC 52 helpers (upstream test literals)
// ---------------------------------------------------------------------------

/// SGR mouse report (upstream `\x1b[<{b};{x};{y}M` press / `\x1b[<{b};{x};{y}m`
/// release literals in tui-alt-screen.test.ts).
pub(crate) fn sgr_mouse(button: u32, x: u32, y: u32, release: bool) -> String {
    format!("\x1b[<{button};{x};{y}{}", if release { 'm' } else { 'M' })
}

/// SGR wheel report: button 64 = up, 65 = down.
pub(crate) fn sgr_wheel(x: u32, y: u32, down: bool) -> String {
    sgr_mouse(if down { 65 } else { 64 }, x, y, false)
}

/// X10 wheel report (`\x1b[M{b}{x}{y}` with each value offset by 32):
/// button 64 = up, 65 = down.
pub(crate) fn x10_wheel(x: u32, y: u32, down: bool) -> String {
    let button = if down { 65 } else { 64 };
    format!(
        "\x1b[M{}{}{}",
        (button + 32) as u8 as char,
        (x + 32) as u8 as char,
        (y + 32) as u8 as char,
    )
}

/// `Buffer.from(text).toString("base64")` equivalent for building expected
/// sequences.
pub(crate) fn base64_encode(data: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// The full OSC 52 clipboard sequence for `text` (upstream tests build this
/// with `Buffer.from(text).toString("base64")`).
pub(crate) fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text))
}

/// Decode every `\x1b]52;c;{base64}\x07` clipboard payload in concatenated
/// writes (upstream `events.filter(write).map(data).includes(expected)`).
pub(crate) fn osc52_payloads(writes: &str) -> Vec<String> {
    let prefix = "\x1b]52;c;";
    let mut payloads = Vec::new();
    let mut rest = writes;
    while let Some(start) = rest.find(prefix) {
        let after = &rest[start + prefix.len()..];
        let end = after.find('\x07').unwrap_or(after.len());
        let payload = &after[..end];
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(payload) {
            payloads.push(String::from_utf8_lossy(&decoded).into_owned());
        }
        rest = &after[end..];
    }
    payloads
}

// ---------------------------------------------------------------------------
// Self-tests for the T31 additions (the T28 main-screen parity behavior is
// exercised by the tui_main_screen suite against this emulator).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autowrap_enabled_by_default_wraps_past_the_last_column() {
        let mut vt = VirtualTerminal::new(5, 2);
        vt.write("abcde");
        // Pending wrap after the last column, like xterm.
        assert_eq!(vt.get_cursor_position(), (5, 0));
        vt.write("X");
        assert_eq!(vt.get_viewport(), vec!["abcde", "X"]);
    }

    #[test]
    fn autowrap_disabled_keeps_the_cursor_on_the_last_column() {
        let mut vt = VirtualTerminal::new(5, 2);
        vt.write("\x1b[?7l");
        vt.write("abcde");
        // The cursor stays on the right margin instead of pending wrap.
        assert_eq!(vt.get_cursor_position(), (4, 0));
        vt.write("XYZ");
        // Each char overwrites the last column; no wrap, no scroll.
        assert_eq!(vt.get_viewport(), vec!["abcdZ", ""]);
        assert_eq!(vt.get_cursor_position(), (4, 0));
        // `?7h` restores wrapping: the next char lands on the right margin
        // and re-enters the pending-wrap state.
        vt.write("\x1b[?7hX");
        assert_eq!(vt.get_viewport(), vec!["abcdX", ""]);
        assert_eq!(vt.get_cursor_position(), (5, 0));
        vt.write("Y");
        assert_eq!(vt.get_viewport(), vec!["abcdX", "Y"]);
    }

    #[test]
    fn alt_screen_switches_buffers_and_restores_the_main_screen() {
        let mut vt = VirtualTerminal::new(5, 2);
        vt.write("main1\r\nmain2");
        assert_eq!(vt.get_viewport(), vec!["main1", "main2"]);

        vt.write("\x1b[?1049h");
        assert!(vt.alt_screen_active());
        // The alt screen starts blank.
        assert_eq!(vt.get_viewport(), vec!["", ""]);
        assert_eq!(vt.get_cursor_position(), (0, 0));
        // The main screen stays intact behind the alt screen.
        assert_eq!(vt.get_main_screen_viewport(), vec!["main1", "main2"]);

        vt.write("alt");
        assert_eq!(vt.get_viewport(), vec!["alt", ""]);

        vt.write("\x1b[?1049l");
        assert!(!vt.alt_screen_active());
        assert_eq!(vt.get_viewport(), vec!["main1", "main2"]);
        // The saved cursor (pending wrap after "main2") is restored.
        assert_eq!(vt.get_cursor_position(), (5, 1));
    }

    #[test]
    fn repeated_alt_screen_enter_is_idempotent() {
        let mut vt = VirtualTerminal::new(5, 1);
        vt.write("main");
        vt.write("\x1b[?1049h\x1b[?1049h");
        vt.write("alt");
        assert_eq!(vt.get_viewport(), vec!["alt"]);
        vt.write("\x1b[?1049l");
        assert_eq!(vt.get_viewport(), vec!["main"]);
        assert_eq!(vt.get_main_screen_viewport(), vec!["main"]);
    }

    #[test]
    fn mouse_mode_csi_is_ignored_without_screen_effects() {
        let mut vt = VirtualTerminal::new(10, 2);
        vt.write("abc");
        let before = vt.get_viewport();
        let cursor = vt.get_cursor_position();
        vt.write("\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h");
        vt.write("\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l");
        assert_eq!(vt.get_viewport(), before);
        assert_eq!(vt.get_cursor_position(), cursor);
    }

    #[test]
    fn osc8_hyperlinks_do_not_shift_wide_char_columns() {
        let mut vt = VirtualTerminal::new(10, 2);
        assert_eq!((vt.columns(), vt.rows()), (10, 2));
        vt.write("A\x1b]8;;https://example.com\x07界\x1b]133;A\x07\x1b]8;;\x07B");
        // Only the visible text lands on screen: A at 0, 界 at 1-2, B at 3.
        assert_eq!(vt.get_viewport(), vec!["A界B", ""]);
        assert_eq!(vt.get_cursor_position(), (4, 0));
    }

    #[test]
    fn apc_kitty_sequences_are_skipped_without_shifting_columns() {
        let mut vt = VirtualTerminal::new(12, 2);
        vt.write("before\x1b_Ga=T,f=100,i=1,q=2\x1b\\after");
        assert_eq!(vt.get_viewport(), vec!["beforeafter", ""]);
        assert_eq!(vt.get_cursor_position(), (11, 0));
    }

    #[test]
    fn osc52_helpers_round_trip_clipboard_payloads() {
        let sequence = osc52_sequence("alpha\nbeta");
        assert_eq!(
            sequence,
            format!("\x1b]52;c;{}\x07", base64_encode("alpha\nbeta"))
        );
        let mut vt = VirtualTerminal::new(10, 2);
        vt.write(&sequence);
        // The emulator skips the OSC payload entirely.
        assert_eq!(vt.get_viewport(), vec!["", ""]);
        assert_eq!(osc52_payloads(&vt.get_writes()), vec!["alpha\nbeta"]);
    }

    #[test]
    fn mouse_input_helpers_emit_upstream_sequences() {
        assert_eq!(sgr_mouse(0, 2, 1, false), "\x1b[<0;2;1M");
        assert_eq!(sgr_mouse(0, 2, 1, true), "\x1b[<0;2;1m");
        assert_eq!(sgr_wheel(1, 6, false), "\x1b[<64;1;6M");
        assert_eq!(sgr_wheel(1, 6, true), "\x1b[<65;1;6M");
        assert_eq!(x10_wheel(1, 1, false), "\x1b[M`!!");
        assert_eq!(x10_wheel(1, 6, true), "\x1b[Ma!&");
    }

    #[test]
    fn recording_terminal_records_start_write_stop_in_order() {
        let mut terminal = RecordingTerminal::new(10, 2);
        terminal.start(Box::new(|_| {}), Box::new(|| {}));
        terminal.write("\x1b[?1049h");
        terminal.stop();
        assert_eq!(
            terminal.events(),
            &[
                VtEvent::Start,
                VtEvent::Write("\x1b[?1049h".to_string()),
                VtEvent::Stop,
            ]
        );
        // Only explicit writes land in the event stream; the bracketed-paste
        // sequences from start/stop go to the inner terminal like upstream.
        assert_eq!(terminal.writes(), "\x1b[?1049h");
        // Deref keeps the screen-model assertions available.
        terminal.write("alt");
        assert_eq!(terminal.get_viewport()[0], "alt");
    }
}
