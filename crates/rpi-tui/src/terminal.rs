//! Port of `packages/tui/src/terminal.ts` @ pi 0.82.1 (2efa728).
//!
//! [`Terminal`] trait (upstream `Terminal` interface) + [`ProcessTerminal`]
//! (upstream `ProcessTerminal`): raw mode, stdin reading, Kitty keyboard
//! protocol negotiation with modifyOtherKeys fallback (DA response without a
//! Kitty answer falls back immediately, no startup timeout), bracketed paste,
//! OSC 9;4 taskbar progress, and terminal state restore on `stop()`.
//!
//! Intentional differences:
//! - Timers become explicit deadlines, matching `StdinBuffer`
//!   (`stdin_buffer.rs`): the 150ms keyboard-protocol-fragment flush timer
//!   (upstream `keyboardProtocolBufferFlushTimer`) and the 1s OSC 9;4
//!   progress keepalive (upstream `progressInterval`) are exposed via
//!   [`Terminal::next_flush_deadline`] / [`Terminal::tick`] and driven by the
//!   TUI event loop. [`Terminal::pump`] combines the channel wait, event
//!   dispatch and deadline firing in one call. Deadline semantics match
//!   upstream: `process()`-driven input reschedules, expiry fires once.
//! - Input routing: upstream's `process.stdin.on("data")` /
//!   `process.stdout.on("resize")` wiring becomes a stdin reader thread
//!   (blocking `StdinLock::read` + incremental UTF-8 decode, mirroring Node's
//!   `setEncoding("utf8")` string decoder) that forwards raw chunks over a
//!   `std::sync::mpsc` channel; `pump()` receives them and dispatches to the
//!   `on_input` / `on_resize` callbacks injected into `start()`. A standard
//!   mpsc channel is used instead of `tokio::sync::mpsc` because `pump()` is
//!   synchronous and needs `recv_timeout` keyed to the flush deadlines
//!   (tokio's `blocking_recv` panics inside an async context and has no
//!   timeout variant); tokio is used where the port is genuinely async —
//!   `drain_input()` and the SIGWINCH forwarder.
//! - Resize: on unix, SIGWINCH is forwarded by a tokio task when `start()` is
//!   called inside a tokio runtime; without a runtime no resize events are
//!   delivered. Upstream's Windows console resize events (libuv `resize`)
//!   and the self-SIGWINCH dimension refresh (terminal.ts:152-156) are not
//!   ported — crossterm's `terminal::size()` queries the ioctl on every
//!   call, so dimensions can never go stale across suspend/resume.
//! - Raw mode: the previous state is captured with crossterm's
//!   `is_raw_mode_enabled()` (upstream `process.stdin.isRaw || false`).
//!   `process.stdin.pause()` in `stop()` (terminal.ts:446) has no Rust
//!   equivalent: the reader thread stays blocked in `read()` after `stop()`
//!   until the next input byte arrives, at which point its channel send
//!   fails and the thread exits.
//! - Windows `ENABLE_VIRTUAL_TERMINAL_INPUT`: upstream loads a native Node
//!   helper (`win32-console-mode.node`, terminal.ts:338-366); crossterm's
//!   raw mode does not set this flag and no native helper is bundled, so on
//!   Windows Shift+Tab arrives as plain `\t` (same gap class as
//!   `native_modifiers.rs`).
//! - Apple Terminal Shift+Enter normalization is wired exactly like upstream
//!   (`forwardInputSequence`, terminal.ts:309-318), but
//!   `is_native_modifier_pressed` always returns `false` in this port (see
//!   `native_modifiers.rs`), so the normalization can never fire on macOS
//!   until a native binding exists.
//! - `drain_input` observes pending input by consuming and discarding queued
//!   channel events; upstream attaches a side listener that only timestamps
//!   arrivals (terminal.ts:385-389). Both leave the drained bytes unhandled;
//!   the port additionally drops them (the process is exiting).
//! - `drainInput` returns a boxed `Future` (async, tokio sleeps) so the
//!   [`Terminal`] trait stays object-safe.
//! - The write log env var is renamed `RPI_TUI_WRITE_LOG` (ADR-0001; upstream
//!   `PI_TUI_WRITE_LOG`, terminal.ts:112); the timestamped directory filename
//!   uses UTC because no local-timezone crate is a dependency. Internal
//!   protocol writes (negotiation, queries, disable sequences) bypass the
//!   write log exactly like upstream, which calls `process.stdout.write`
//!   directly for them instead of `this.write()`.
//! - Kitty flags parsing saturates at `u32::MAX` instead of using JS doubles;
//!   the only observed property (`flags !== 0`) is preserved.

use std::borrow::Cow;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::keys::set_kitty_protocol_active;
use crate::native_modifiers::{is_native_modifier_pressed, ModifierKey};
use crate::stdin_buffer::{StdinBuffer, StdinBufferEvent};

/// `TERMINAL_PROGRESS_KEEPALIVE_MS` (terminal.ts:11).
const TERMINAL_PROGRESS_KEEPALIVE: Duration = Duration::from_millis(1000);
/// `TERMINAL_PROGRESS_ACTIVE_SEQUENCE` (terminal.ts:12).
const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\x1b]9;4;3\x07";
/// `TERMINAL_PROGRESS_CLEAR_SEQUENCE` (terminal.ts:13).
const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\x1b]9;4;0;\x07";
/// `APPLE_TERMINAL_SHIFT_ENTER_SEQUENCE` (terminal.ts:14).
const APPLE_TERMINAL_SHIFT_ENTER_SEQUENCE: &str = "\x1b[13;2u";
/// `DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS` (terminal.ts:15):
/// 1 = disambiguate escape codes, 2 = report event types
/// (press/repeat/release), 4 = report alternate keys (shifted key, base
/// layout key).
const DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS: u8 = 7;
/// `KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT_MS` (terminal.ts:16).
const KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);
/// `PI_TUI_WRITE_LOG` (terminal.ts:112) — Rpi rename (ADR-0001), mirrored
/// from `rpi::core::environment::ENV_TUI_WRITE_LOG` (rpi-tui cannot depend
/// on rpi, coding-standards §2.2).
const ENV_TUI_WRITE_LOG: &str = "RPI_TUI_WRITE_LOG";

/// `KeyboardProtocolNegotiationSequence` (terminal.ts:19-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    /// `{ type: "kitty-flags"; flags: number }`.
    KittyFlags { flags: u32 },
    /// `{ type: "device-attributes" }`.
    DeviceAttributes,
}

/// `parseKeyboardProtocolNegotiationSequence` (terminal.ts:23-34).
pub fn parse_keyboard_protocol_negotiation_sequence(
    sequence: &str,
) -> Option<KeyboardProtocolNegotiationSequence> {
    // /^\x1b\[\?(\d+)u$/ (JS `\d` is ASCII digits). The flags value saturates
    // at u32::MAX for absurdly long digit runs where upstream would produce a
    // large double; only `flags !== 0` is ever observed.
    if let Some(digits) = sequence
        .strip_prefix("\x1b[?")
        .and_then(|rest| rest.strip_suffix('u'))
    {
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let flags = digits.parse::<u32>().unwrap_or(u32::MAX);
            return Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags });
        }
    }
    // /^\x1b\[\?[\d;]*c$/
    if let Some(middle) = sequence
        .strip_prefix("\x1b[?")
        .and_then(|rest| rest.strip_suffix('c'))
    {
        if middle.bytes().all(|b| b.is_ascii_digit() || b == b';') {
            return Some(KeyboardProtocolNegotiationSequence::DeviceAttributes);
        }
    }
    None
}

/// `isKeyboardProtocolNegotiationSequencePrefix` (terminal.ts:36-38).
fn is_keyboard_protocol_negotiation_sequence_prefix(sequence: &str) -> bool {
    if sequence == "\x1b[" {
        return true;
    }
    // /^\x1b\[\?[\d;]*$/
    match sequence.strip_prefix("\x1b[?") {
        Some(rest) => rest.bytes().all(|b| b.is_ascii_digit() || b == b';'),
        None => false,
    }
}

/// `isAppleTerminalSession` (terminal.ts:40-42).
pub fn is_apple_terminal_session() -> bool {
    cfg!(target_os = "macos") && std::env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal")
}

/// `normalizeAppleTerminalInput` (terminal.ts:44-47).
pub fn normalize_apple_terminal_input<'a>(
    data: &'a str,
    is_apple_terminal: bool,
    is_shift_pressed: bool,
) -> Cow<'a, str> {
    if is_apple_terminal && data == "\r" && is_shift_pressed {
        return Cow::Borrowed(APPLE_TERMINAL_SHIFT_ENTER_SEQUENCE);
    }
    Cow::Borrowed(data)
}

/// Input handler injected into [`Terminal::start`] (upstream `onInput`).
pub type InputHandler = Box<dyn FnMut(&str) + Send>;

/// Resize handler injected into [`Terminal::start`] (upstream `onResize`).
pub type ResizeHandler = Box<dyn FnMut() + Send>;

/// Minimal terminal interface for the TUI (upstream `Terminal`,
/// terminal.ts:52-94).
pub trait Terminal {
    /// Start the terminal with input and resize handlers (upstream
    /// `start(onInput, onResize)`).
    fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler);

    /// Stop the terminal and restore state (upstream `stop()`).
    fn stop(&mut self);

    /// Drain stdin before exiting to prevent Kitty key release events from
    /// leaking to the parent shell over slow SSH connections (upstream
    /// `drainInput(maxMs?, idleMs?)`).
    ///
    /// - `max_ms` - maximum time to drain (default: 1000ms)
    /// - `idle_ms` - exit early if no input arrives within this time (default: 50ms)
    ///
    /// The returned future must be polled inside a Tokio runtime context:
    /// the implementation awaits `tokio::time::sleep`, which panics without
    /// one.
    ///
    /// Boxed future so the trait stays object-safe.
    fn drain_input(
        &mut self,
        max_ms: Option<u64>,
        idle_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Write output to the terminal (upstream `write(data)`).
    fn write(&mut self, data: &str);

    /// Terminal dimensions (upstream `get columns()` / `get rows()`).
    fn columns(&self) -> u16;
    fn rows(&self) -> u16;

    /// Whether the Kitty keyboard protocol is active (upstream
    /// `get kittyProtocolActive()`).
    fn kitty_protocol_active(&self) -> bool;

    /// Move cursor up (negative) or down (positive) by N lines (upstream
    /// `moveBy(lines)`).
    fn move_by(&mut self, lines: i32);

    /// Hide the cursor (upstream `hideCursor()`).
    fn hide_cursor(&mut self);
    /// Show the cursor (upstream `showCursor()`).
    fn show_cursor(&mut self);

    /// Clear the current line (upstream `clearLine()`).
    fn clear_line(&mut self);
    /// Clear from cursor to end of screen (upstream `clearFromCursor()`).
    fn clear_from_cursor(&mut self);
    /// Clear the entire screen and move cursor to (0,0) (upstream
    /// `clearScreen()`).
    fn clear_screen(&mut self);

    /// Set the terminal window title (upstream `setTitle(title)`).
    fn set_title(&mut self, title: &str);

    /// Progress indicator (OSC 9;4) (upstream `setProgress(active)`).
    fn set_progress(&mut self, active: bool);

    /// Rust addition (explicit-deadline replacement for the upstream timers,
    /// see header note): the next instant at which [`Terminal::tick`] has
    /// work to do — StdinBuffer tail flush, keyboard-protocol fragment flush
    /// or progress keepalive. `None` when no deadline is pending.
    fn next_flush_deadline(&self) -> Option<Instant> {
        None
    }

    /// Rust addition: fire every deadline that has expired by `now`
    /// (StdinBuffer flush routed through negotiation, keyboard-protocol
    /// fragment flush forwarded as plain input, progress keepalive rewrite).
    /// Called by [`Terminal::pump`]; also usable standalone.
    fn tick(&mut self, _now: Instant) {}

    /// Rust addition: wait up to `timeout` (`None` = indefinitely) for a
    /// terminal event, dispatch pending events to the `start()` handlers,
    /// then fire expired deadlines via [`Terminal::tick`]. Returns `true` if
    /// at least one event was dispatched.
    fn pump(&mut self, _timeout: Option<Duration>) -> bool {
        false
    }

    /// Lock-free wait handle for the terminal's event stream, available
    /// after `start`. Lets [`Tui::pump`](crate::tui::Tui::pump) block on
    /// input WITHOUT holding the terminal lock, so the driver's parked wait
    /// cannot starve blocking lockers on other threads (see the
    /// `SharedTerminal` note in tui.rs). `None` for terminals without a
    /// channel-backed event stream (e.g. the virtual test terminal).
    #[doc(hidden)]
    fn event_source(&self) -> Option<TerminalEventSource> {
        None
    }

    /// Dispatch an event previously obtained from the [`Terminal::event_source`]
    /// stream (the [`Tui::pump`](crate::tui::Tui::pump) path).
    #[doc(hidden)]
    fn dispatch_terminal_event(&mut self, event: TerminalEvent) {
        let _ = event;
    }
}

/// Raw events produced by the stdin reader thread / SIGWINCH forwarder and
/// dispatched by [`ProcessTerminal::pump`]. `pub` only so [`Tui::pump`]
/// (tui.rs) can wait on the event stream without holding the terminal lock;
/// not part of the supported API.
///
/// [`Tui::pump`]: crate::tui::Tui::pump
#[doc(hidden)]
#[derive(Debug)]
pub enum TerminalEvent {
    /// A chunk of stdin decoded as UTF-8 (upstream `data` event string).
    Input(String),
    /// SIGWINCH (upstream `resize` event).
    Resize,
}

/// Cloneable wait handle for a terminal's event stream (the receiving end of
/// the channel fed by the stdin reader thread / SIGWINCH forwarder). Lets
/// the TUI's driver block on input WITHOUT holding the terminal lock: the
/// driver parks here between frames, and holding a lock across that wait
/// starves blocking lockers on other threads (`std::sync::Mutex` is not
/// FIFO-fair; see the `SharedTerminal` note in tui.rs).
///
/// `pub` only as part of the [`TerminalEvent`] plumbing; not supported API.
#[doc(hidden)]
#[derive(Clone)]
pub struct TerminalEventSource {
    // `Mutex` because `mpsc::Receiver` is `Send` but not `Sync`; the source
    // lives inside `ProcessTerminal`, which must stay `Send` for
    // `drain_input`'s boxed future.
    rx: std::sync::Arc<std::sync::Mutex<mpsc::Receiver<TerminalEvent>>>,
}

impl TerminalEventSource {
    fn new(rx: mpsc::Receiver<TerminalEvent>) -> Self {
        TerminalEventSource {
            rx: std::sync::Arc::new(std::sync::Mutex::new(rx)),
        }
    }

    /// Wait up to `timeout` (`None` = indefinitely) for the next event.
    pub fn wait(&self, timeout: Option<Duration>) -> Option<TerminalEvent> {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        match timeout {
            Some(limit) => rx.recv_timeout(limit).ok(),
            None => rx.recv().ok(),
        }
    }

    /// Take an already-queued event, if any.
    pub fn try_recv(&self) -> Option<TerminalEvent> {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        rx.try_recv().ok()
    }
}

/// Result of [`ProcessTerminal::read_keyboard_protocol_negotiation_sequence`];
/// the upstream `KeyboardProtocolNegotiationSequence | "pending" | undefined`
/// union (terminal.ts:254).
enum NegotiationRead {
    Pending,
    Sequence(KeyboardProtocolNegotiationSequence),
    NotNegotiation,
}

/// Real terminal on process stdin/stdout (upstream `ProcessTerminal`,
/// terminal.ts:99-531), based on crossterm for raw mode and size queries.
///
/// Generic over the output sink so tests can capture the raw byte stream
/// (upstream tests monkey-patch `process.stdout.write` instead); production
/// uses `ProcessTerminal<io::Stdout>` via [`ProcessTerminal::new`].
pub struct ProcessTerminal<W: Write = io::Stdout> {
    out: W,
    write_log_path: Option<PathBuf>,
    was_raw: bool,
    /// Raw mode was toggled by `start()`; guards raw restore in `stop()`
    /// (upstream calls `setRawMode` unconditionally, which is a no-op there
    /// when stdin was never resumed).
    started: bool,
    input_handler: Option<InputHandler>,
    resize_handler: Option<ResizeHandler>,
    kitty_protocol_active: bool,
    modify_other_keys_active: bool,
    keyboard_protocol_pushed: bool,
    keyboard_protocol_negotiation_buffer: String,
    /// Explicit-deadline form of `keyboardProtocolBufferFlushTimer`.
    keyboard_protocol_negotiation_flush_deadline: Option<Instant>,
    stdin_buffer: Option<StdinBuffer>,
    /// Explicit-deadline form of `progressInterval`.
    progress_keepalive_deadline: Option<Instant>,
    event_tx: Option<mpsc::Sender<TerminalEvent>>,
    event_rx: Option<TerminalEventSource>,
    resize_task: Option<tokio::task::JoinHandle<()>>,
    /// Terminal size query; injectable so tests can simulate "not a tty"
    /// (upstream tests monkey-patch `process.stdout.columns/rows`).
    query_size: fn() -> io::Result<(u16, u16)>,
}

impl ProcessTerminal<io::Stdout> {
    /// Terminal on process stdin/stdout (upstream `new ProcessTerminal()`).
    pub fn new() -> Self {
        Self::with_writer(io::stdout())
    }
}

impl Default for ProcessTerminal<io::Stdout> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: Write> ProcessTerminal<W> {
    /// Terminal writing to a custom sink (test injection point).
    pub fn with_writer(out: W) -> Self {
        Self {
            out,
            write_log_path: resolve_write_log_path(),
            was_raw: false,
            started: false,
            input_handler: None,
            resize_handler: None,
            kitty_protocol_active: false,
            modify_other_keys_active: false,
            keyboard_protocol_pushed: false,
            keyboard_protocol_negotiation_buffer: String::new(),
            keyboard_protocol_negotiation_flush_deadline: None,
            stdin_buffer: None,
            progress_keepalive_deadline: None,
            event_tx: None,
            event_rx: None,
            resize_task: None,
            query_size: crossterm::terminal::size,
        }
    }

    /// Upstream `get modifyOtherKeysActive()` (terminal.ts:130-132).
    pub fn modify_other_keys_active(&self) -> bool {
        self.modify_other_keys_active
    }

    /// Query the terminal for Kitty keyboard protocol support and enable it
    /// if available (upstream `queryAndEnableKittyProtocol`,
    /// terminal.ts:220-226).
    ///
    /// Kitty's progressive enhancement detection requires requesting the
    /// desired flags before querying them. The trailing DA query is a
    /// sentinel supported by terminals that do not know Kitty keyboard
    /// protocol; receiving DA before a Kitty response enables the
    /// modifyOtherKeys fallback without a startup timeout.
    ///
    /// The upstream `process.stdin.on("data", ...)` wiring corresponds to the
    /// reader thread + channel set up in `start()`.
    fn query_and_enable_kitty_protocol(&mut self) {
        // `setupStdinBuffer` (terminal.ts:177-205): `new StdinBuffer({ timeout: 10 })`
        // (10ms is the StdinBuffer default). The upstream `data`/`paste`
        // handlers correspond to `handle_stdin_data` / `route_data_sequence`.
        self.stdin_buffer = Some(StdinBuffer::default());
        self.keyboard_protocol_pushed = true;
        self.clear_keyboard_protocol_negotiation_buffer();
        // `KITTY_KEYBOARD_PROTOCOL_QUERY` (terminal.ts:17).
        self.write_direct(&format!(
            "\x1b[>{}u\x1b[?u\x1b[c",
            DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS
        ));
    }

    /// Blocking stdin reader thread: raw bytes → incremental UTF-8 decode →
    /// channel (upstream `process.stdin` `data` events after
    /// `setEncoding("utf8")` + `resume()`).
    fn spawn_stdin_reader(&mut self) {
        let Some(tx) = self.event_tx.clone() else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("rpi-tui-stdin".to_string())
            .spawn(move || {
                let mut stdin = io::stdin().lock();
                let mut buf = [0u8; 65_536];
                let mut pending: Vec<u8> = Vec::new();
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            pending.extend_from_slice(&buf[..n]);
                            let (text, rest) = decode_utf8_incremental(&pending);
                            pending = rest;
                            if !text.is_empty() && tx.send(TerminalEvent::Input(text)).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
        // Detach: the thread exits on EOF, on read error, or when its next
        // channel send fails after `stop()` dropped the receiver.
        drop(spawned);
    }

    /// SIGWINCH → channel forwarder (upstream
    /// `process.stdout.on("resize", ...)`). Requires a tokio runtime at
    /// `start()` time; without one, resize events are not delivered (see
    /// header note).
    #[cfg(unix)]
    fn spawn_resize_forwarder(&mut self) {
        let Some(tx) = self.event_tx.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.resize_task = Some(runtime.spawn(async move {
            let Ok(mut sigwinch) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            else {
                return;
            };
            while sigwinch.recv().await.is_some() {
                if tx.send(TerminalEvent::Resize).is_err() {
                    break;
                }
            }
        }));
    }

    /// Upstream relies on libuv console `resize` events on Windows; no
    /// equivalent is wired in this port (see header note).
    #[cfg(not(unix))]
    fn spawn_resize_forwarder(&mut self) {}

    /// Dispatch one channel event to the registered handlers.
    fn dispatch_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::Input(data) => self.handle_stdin_data(&data),
            TerminalEvent::Resize => {
                if let Some(handler) = self.resize_handler.as_mut() {
                    handler();
                }
            }
        }
    }

    /// Upstream `stdinDataHandler` (terminal.ts:202-204): pipe stdin data
    /// through the StdinBuffer, then route the emitted events.
    fn handle_stdin_data(&mut self, data: &str) {
        let events = match self.stdin_buffer.as_mut() {
            Some(buffer) => buffer.process(data),
            None => return,
        };
        for event in events {
            match event {
                StdinBufferEvent::Data(sequence) => self.route_data_sequence(&sequence),
                // Re-wrap paste content with bracketed paste markers for the
                // existing editor handling (terminal.ts:195-199).
                StdinBufferEvent::Paste(content) => {
                    if let Some(handler) = self.input_handler.as_mut() {
                        handler(&format!("\x1b[200~{content}\x1b[201~"));
                    }
                }
            }
        }
    }

    /// Upstream StdinBuffer `data` handler (terminal.ts:181-192): watch for
    /// the Kitty protocol response and forward everything else as input.
    fn route_data_sequence(&mut self, sequence: &str) {
        match self.read_keyboard_protocol_negotiation_sequence(sequence) {
            // Wait briefly for the rest of a split Kitty response.
            NegotiationRead::Pending => self.schedule_keyboard_protocol_negotiation_buffer_flush(),
            NegotiationRead::Sequence(negotiation_sequence) => {
                self.handle_keyboard_protocol_negotiation_sequence(negotiation_sequence);
            }
            NegotiationRead::NotNegotiation => self.forward_input_sequence(sequence),
        }
    }

    /// Upstream `handleKeyboardProtocolNegotiationSequence`
    /// (terminal.ts:228-250); the upstream `undefined → false` case
    /// corresponds to [`NegotiationRead::NotNegotiation`].
    fn handle_keyboard_protocol_negotiation_sequence(
        &mut self,
        negotiation_sequence: KeyboardProtocolNegotiationSequence,
    ) {
        self.clear_keyboard_protocol_negotiation_buffer();
        match negotiation_sequence {
            KeyboardProtocolNegotiationSequence::KittyFlags { flags } => {
                if flags != 0 {
                    self.disable_modify_other_keys();
                    if !self.kitty_protocol_active {
                        self.kitty_protocol_active = true;
                        set_kitty_protocol_active(true);
                    }
                } else {
                    self.enable_modify_other_keys();
                }
            }
            KeyboardProtocolNegotiationSequence::DeviceAttributes => {
                if !self.kitty_protocol_active {
                    self.enable_modify_other_keys();
                }
            }
        }
    }

    /// Upstream `readKeyboardProtocolNegotiationSequence`
    /// (terminal.ts:252-276).
    fn read_keyboard_protocol_negotiation_sequence(&mut self, sequence: &str) -> NegotiationRead {
        if !self.keyboard_protocol_negotiation_buffer.is_empty() {
            let buffered_sequence =
                format!("{}{sequence}", self.keyboard_protocol_negotiation_buffer);
            if let Some(negotiation_sequence) =
                parse_keyboard_protocol_negotiation_sequence(&buffered_sequence)
            {
                self.clear_keyboard_protocol_negotiation_buffer();
                return NegotiationRead::Sequence(negotiation_sequence);
            }
            if is_keyboard_protocol_negotiation_sequence_prefix(&buffered_sequence) {
                self.set_keyboard_protocol_negotiation_buffer(buffered_sequence);
                return NegotiationRead::Pending;
            }
            self.flush_keyboard_protocol_negotiation_buffer_as_input();
        }

        if let Some(negotiation_sequence) = parse_keyboard_protocol_negotiation_sequence(sequence) {
            return NegotiationRead::Sequence(negotiation_sequence);
        }
        if is_keyboard_protocol_negotiation_sequence_prefix(sequence) {
            self.set_keyboard_protocol_negotiation_buffer(sequence.to_string());
            return NegotiationRead::Pending;
        }
        NegotiationRead::NotNegotiation
    }

    /// Upstream `setKeyboardProtocolNegotiationBuffer` (terminal.ts:278-281).
    fn set_keyboard_protocol_negotiation_buffer(&mut self, sequence: String) {
        // `clearKeyboardProtocolNegotiationBufferFlushTimer` becomes clearing
        // the explicit deadline.
        self.keyboard_protocol_negotiation_flush_deadline = None;
        self.keyboard_protocol_negotiation_buffer = sequence;
    }

    /// Upstream `clearKeyboardProtocolNegotiationBuffer` (terminal.ts:283-286).
    fn clear_keyboard_protocol_negotiation_buffer(&mut self) {
        self.keyboard_protocol_negotiation_flush_deadline = None;
        self.keyboard_protocol_negotiation_buffer.clear();
    }

    /// Upstream `flushKeyboardProtocolNegotiationBufferAsInput`
    /// (terminal.ts:288-293).
    fn flush_keyboard_protocol_negotiation_buffer_as_input(&mut self) {
        if self.keyboard_protocol_negotiation_buffer.is_empty() {
            return;
        }
        let sequence = std::mem::take(&mut self.keyboard_protocol_negotiation_buffer);
        self.keyboard_protocol_negotiation_flush_deadline = None;
        self.forward_input_sequence(&sequence);
    }

    /// Upstream `scheduleKeyboardProtocolNegotiationBufferFlush`
    /// (terminal.ts:295-301); the 150ms `setTimeout` becomes an explicit
    /// deadline fired by `tick()`.
    fn schedule_keyboard_protocol_negotiation_buffer_flush(&mut self) {
        if self.keyboard_protocol_negotiation_buffer.is_empty()
            || self.keyboard_protocol_negotiation_flush_deadline.is_some()
        {
            return;
        }
        self.keyboard_protocol_negotiation_flush_deadline =
            Some(Instant::now() + KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT);
    }

    /// Upstream `forwardInputSequence` (terminal.ts:309-318).
    fn forward_input_sequence(&mut self, sequence: &str) {
        let Some(handler) = self.input_handler.as_mut() else {
            return;
        };
        let is_apple_terminal = sequence == "\r" && is_apple_terminal_session();
        let input = normalize_apple_terminal_input(
            sequence,
            is_apple_terminal,
            is_apple_terminal && is_native_modifier_pressed(ModifierKey::Shift),
        );
        handler(&input);
    }

    /// Upstream `enableModifyOtherKeys` (terminal.ts:320-324).
    fn enable_modify_other_keys(&mut self) {
        if self.kitty_protocol_active || self.modify_other_keys_active {
            return;
        }
        self.write_direct("\x1b[>4;2m");
        self.modify_other_keys_active = true;
    }

    /// Upstream `disableModifyOtherKeys` (terminal.ts:326-330).
    fn disable_modify_other_keys(&mut self) {
        if !self.modify_other_keys_active {
            return;
        }
        self.write_direct("\x1b[>4;0m");
        self.modify_other_keys_active = false;
    }

    /// Upstream `clearProgressInterval` (terminal.ts:525-530): returns
    /// whether a keepalive was active.
    fn clear_progress_keepalive(&mut self) -> bool {
        self.progress_keepalive_deadline.take().is_some()
    }

    /// Write to the output sink without touching the write log. Upstream
    /// internal writes call `process.stdout.write` directly; only the public
    /// `write()` appends to the log (terminal.ts:454-463).
    fn write_direct(&mut self, data: &str) {
        let _ = self.out.write_all(data.as_bytes());
        // Rust's Stdout is line-buffered; escape sequences carry no newline,
        // so flush every write to match Node's immediate stdout writes.
        let _ = self.out.flush();
    }
}

impl<W: Write + Send> Terminal for ProcessTerminal<W> {
    fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
        self.input_handler = Some(on_input);
        self.resize_handler = Some(on_resize);

        // Save previous state and enable raw mode.
        self.was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        let _ = crossterm::terminal::enable_raw_mode();
        self.started = true;

        // Enable bracketed paste mode - terminal will wrap pastes in
        // \x1b[200~ ... \x1b[201~.
        self.write_direct("\x1b[?2004h");

        let (tx, rx) = mpsc::channel();
        self.event_tx = Some(tx);
        self.event_rx = Some(TerminalEventSource::new(rx));

        // Resize handler wiring (upstream `process.stdout.on("resize")`).
        self.spawn_resize_forwarder();

        // Upstream re-sends SIGWINCH to itself to refresh stale dimensions
        // after suspend/resume (terminal.ts:152-156); crossterm queries the
        // ioctl on every `size()` call, so there is no stale cache to
        // refresh.
        //
        // Upstream `enableWindowsVTInput` (terminal.ts:338-366) loads a
        // native Node helper to set ENABLE_VIRTUAL_TERMINAL_INPUT; no
        // equivalent binding is bundled in this port (see header note).

        self.query_and_enable_kitty_protocol();
        self.spawn_stdin_reader();
    }

    fn stop(&mut self) {
        if self.clear_progress_keepalive() {
            self.write_direct(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }

        // Disable bracketed paste mode.
        self.write_direct("\x1b[?2004l");

        let should_disable_kitty_protocol =
            self.keyboard_protocol_pushed || self.kitty_protocol_active;
        self.clear_keyboard_protocol_negotiation_buffer();

        // Disable Kitty keyboard protocol if not already done by drain_input().
        if should_disable_kitty_protocol {
            self.write_direct("\x1b[<u");
            self.keyboard_protocol_pushed = false;
            self.kitty_protocol_active = false;
            set_kitty_protocol_active(false);
        }
        self.disable_modify_other_keys();

        // Clean up the StdinBuffer (upstream `destroy()`).
        if let Some(mut buffer) = self.stdin_buffer.take() {
            buffer.destroy();
        }

        // Remove event handlers / event sources.
        self.input_handler = None;
        self.resize_handler = None;
        if let Some(task) = self.resize_task.take() {
            task.abort();
        }
        self.event_rx = None;
        self.event_tx = None;

        // Upstream pauses stdin to prevent buffered input (e.g. Ctrl+D) from
        // being re-interpreted after raw mode is disabled (terminal.ts:443-446).
        // No Rust equivalent: the reader thread stays blocked in `read()`
        // until the next input byte arrives, then exits because its channel
        // send fails (see header note).

        // Restore raw mode state.
        if self.started {
            let _ = if self.was_raw {
                crossterm::terminal::enable_raw_mode()
            } else {
                crossterm::terminal::disable_raw_mode()
            };
            self.started = false;
        }
    }

    fn drain_input(
        &mut self,
        max_ms: Option<u64>,
        idle_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Upstream defaults: `drainInput(maxMs = 1000, idleMs = 50)`.
        let max = Duration::from_millis(max_ms.unwrap_or(1000));
        let idle = Duration::from_millis(idle_ms.unwrap_or(50));
        Box::pin(async move {
            let should_disable_kitty_protocol =
                self.keyboard_protocol_pushed || self.kitty_protocol_active;
            self.clear_keyboard_protocol_negotiation_buffer();
            if should_disable_kitty_protocol {
                // Disable Kitty keyboard protocol first so any late key
                // releases do not generate new Kitty escape sequences.
                self.write_direct("\x1b[<u");
                self.keyboard_protocol_pushed = false;
                self.kitty_protocol_active = false;
                set_kitty_protocol_active(false);
            }
            self.disable_modify_other_keys();

            let previous_handler = self.input_handler.take();

            // Upstream attaches a `data` listener that only timestamps
            // arrivals; here queued channel events are consumed and discarded
            // instead (see header note).
            let mut last_data_time = Instant::now();
            let end_time = Instant::now() + max;

            loop {
                if let Some(rx) = &self.event_rx {
                    while rx.try_recv().is_some() {
                        last_data_time = Instant::now();
                    }
                }
                let now = Instant::now();
                let time_left = end_time.saturating_duration_since(now);
                if time_left.is_zero() {
                    break;
                }
                if now.duration_since(last_data_time) >= idle {
                    break;
                }
                tokio::time::sleep(time_left.min(idle)).await;
            }

            self.input_handler = previous_handler;
        })
    }

    fn write(&mut self, data: &str) {
        self.write_direct(data);
        if let Some(path) = &self.write_log_path {
            // `fs.appendFileSync(writeLogPath, data)`; logging errors ignored.
            let result = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| file.write_all(data.as_bytes()));
            let _ = result;
        }
    }

    fn columns(&self) -> u16 {
        // `process.stdout.columns || Number(process.env.COLUMNS) || 80`.
        (self.query_size)()
            .ok()
            .map(|size| size.0)
            .filter(|&columns| columns > 0)
            .or_else(|| env_dimension("COLUMNS"))
            .unwrap_or(80)
    }

    fn rows(&self) -> u16 {
        // `process.stdout.rows || Number(process.env.LINES) || 24`.
        (self.query_size)()
            .ok()
            .map(|size| size.1)
            .filter(|&rows| rows > 0)
            .or_else(|| env_dimension("LINES"))
            .unwrap_or(24)
    }

    fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }

    fn move_by(&mut self, lines: i32) {
        if lines > 0 {
            // Move down.
            self.write_direct(&format!("\x1b[{lines}B"));
        } else if lines < 0 {
            // Move up.
            self.write_direct(&format!("\x1b[{}A", lines.unsigned_abs()));
        }
        // lines == 0: no movement.
    }

    fn hide_cursor(&mut self) {
        self.write_direct("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.write_direct("\x1b[?25h");
    }

    fn clear_line(&mut self) {
        self.write_direct("\x1b[K");
    }

    fn clear_from_cursor(&mut self) {
        self.write_direct("\x1b[J");
    }

    fn clear_screen(&mut self) {
        // Clear screen and move to home (1,1).
        self.write_direct("\x1b[2J\x1b[H");
    }

    fn set_title(&mut self, title: &str) {
        // OSC 0;title BEL - set terminal window title.
        self.write_direct(&format!("\x1b]0;{title}\x07"));
    }

    fn set_progress(&mut self, active: bool) {
        if active {
            // OSC 9;4;3 - indeterminate progress.
            self.write_direct(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            if self.progress_keepalive_deadline.is_none() {
                // The upstream `setInterval` keepalive becomes an explicit
                // deadline fired by `tick()`.
                self.progress_keepalive_deadline =
                    Some(Instant::now() + TERMINAL_PROGRESS_KEEPALIVE);
            }
        } else {
            self.clear_progress_keepalive();
            // OSC 9;4;0 - clear progress.
            self.write_direct(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }

    fn next_flush_deadline(&self) -> Option<Instant> {
        [
            self.stdin_buffer
                .as_ref()
                .and_then(StdinBuffer::flush_deadline),
            self.keyboard_protocol_negotiation_flush_deadline,
            self.progress_keepalive_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn tick(&mut self, now: Instant) {
        // StdinBuffer tail flush: the flushed sequences go through the same
        // negotiation + forwarding path as freshly processed data (upstream
        // stdin-buffer `data` handler, terminal.ts:181-192).
        let flushed = match self.stdin_buffer.as_mut() {
            Some(buffer) => buffer.flush_expired(now),
            None => Vec::new(),
        };
        for sequence in flushed {
            self.route_data_sequence(&sequence);
        }

        // Keyboard-protocol fragment flush (upstream 150ms timer callback,
        // terminal.ts:295-301).
        if self
            .keyboard_protocol_negotiation_flush_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.keyboard_protocol_negotiation_flush_deadline = None;
            self.flush_keyboard_protocol_negotiation_buffer_as_input();
        }

        // OSC 9;4 progress keepalive (upstream `setInterval`,
        // terminal.ts:513-517).
        if self
            .progress_keepalive_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.write_direct(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            self.progress_keepalive_deadline = Some(now + TERMINAL_PROGRESS_KEEPALIVE);
        }
    }

    fn pump(&mut self, timeout: Option<Duration>) -> bool {
        let first = match (&self.event_rx, timeout) {
            (Some(rx), Some(limit)) => rx.wait(Some(limit)),
            (Some(rx), None) => rx.wait(None),
            (None, Some(limit)) => {
                // No input source (start() never ran or stop() already ran):
                // still honor the wait so deadline-driven flushing works.
                std::thread::sleep(limit);
                None
            }
            (None, None) => None,
        };

        let mut dispatched = false;
        if let Some(event) = first {
            self.dispatch_event(event);
            dispatched = true;
        }

        // Drain events that queued up behind the first one.
        let mut queued = Vec::new();
        if let Some(rx) = &self.event_rx {
            while let Some(event) = rx.try_recv() {
                queued.push(event);
            }
        }
        for event in queued {
            self.dispatch_event(event);
            dispatched = true;
        }

        self.tick(Instant::now());
        dispatched
    }

    fn event_source(&self) -> Option<TerminalEventSource> {
        self.event_rx.clone()
    }

    fn dispatch_terminal_event(&mut self, event: TerminalEvent) {
        self.dispatch_event(event);
    }
}

/// `Number(process.env.<name>) || undefined` for terminal dimension
/// fallbacks: unparseable/empty (NaN) and zero are falsy upstream and fall
/// through to the default.
fn env_dimension(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|&value| value > 0)
}

/// Incremental UTF-8 decode matching Node's `setEncoding("utf8")` string
/// decoder: complete input is appended verbatim, invalid byte sequences
/// become U+FFFD, and an incomplete trailing sequence is held back for the
/// next chunk (second tuple element).
fn decode_utf8_incremental(bytes: &[u8]) -> (String, Vec<u8>) {
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                out.push_str(valid);
                return (out, Vec::new());
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    // The prefix up to `valid_up_to` is valid UTF-8 by
                    // construction.
                    if let Ok(prefix) = std::str::from_utf8(&rest[..valid_up_to]) {
                        out.push_str(prefix);
                    }
                }
                match error.error_len() {
                    Some(len) => {
                        out.push('\u{FFFD}');
                        rest = &rest[valid_up_to + len..];
                    }
                    // Incomplete trailing sequence: hold it for the next chunk.
                    None => return (out, rest[valid_up_to..].to_vec()),
                }
            }
        }
    }
}

/// Upstream `writeLogPath` initializer IIFE (terminal.ts:111-124): unset or
/// empty env disables the log; an existing directory gets a timestamped file
/// inside; anything else (including stat errors) is used as the file path.
fn resolve_write_log_path() -> Option<PathBuf> {
    let env = std::env::var(ENV_TUI_WRITE_LOG)
        .ok()
        .filter(|value| !value.is_empty())?;
    let path = PathBuf::from(env);
    if path.is_dir() {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Some(path.join(format!(
            "tui-{}-{}.log",
            utc_timestamp(secs),
            std::process::id()
        )))
    } else {
        Some(path)
    }
}

/// `YYYY-MM-DD_HH-MM-SS` in UTC (upstream uses local time components; no
/// local-timezone crate is a dependency — see header note).
fn utc_timestamp(secs_since_epoch: u64) -> String {
    let days = (secs_since_epoch / 86_400) as i64;
    let secs_of_day = secs_since_epoch % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60,
    );
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097) as u64; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1; // [1, 31]
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}_{hour:02}-{minute:02}-{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Upstream tests monkey-patch `process.stdout.write` to capture chunks;
    /// here the writer is injected. Each `write` call is one captured entry,
    /// mirroring the upstream `writes: string[]`.
    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<String>>>);

    impl SharedWriter {
        fn writes(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // --- normalizeAppleTerminalInput (upstream describe "normalizeAppleTerminalInput") ---

    #[test]
    fn rewrites_apple_terminal_return_to_csi_u_shift_enter_when_shift_pressed() {
        assert_eq!(
            normalize_apple_terminal_input("\r", true, true),
            "\x1b[13;2u"
        );
    }

    #[test]
    fn leaves_apple_terminal_return_unchanged_when_shift_not_pressed() {
        assert_eq!(normalize_apple_terminal_input("\r", true, false), "\r");
    }

    #[test]
    fn leaves_non_apple_terminal_return_unchanged_when_shift_pressed() {
        assert_eq!(normalize_apple_terminal_input("\r", false, true), "\r");
    }

    #[test]
    fn leaves_non_return_input_unchanged() {
        assert_eq!(
            normalize_apple_terminal_input("\x1b[13;2u", true, true),
            "\x1b[13;2u"
        );
        assert_eq!(normalize_apple_terminal_input("a", true, true), "a");
    }

    // --- Kitty keyboard protocol negotiation (upstream describe "ProcessTerminal
    //     Kitty keyboard protocol negotiation") ---

    /// Upstream `NegotiationHarness` (terminal.test.ts:26-83).
    struct NegotiationHarness {
        terminal: ProcessTerminal<SharedWriter>,
        writer: SharedWriter,
        input: Arc<Mutex<Option<String>>>,
    }

    /// Upstream `setupNegotiation`: sets the private input handler and calls
    /// the private `queryAndEnableKittyProtocol` directly (no `start()`, so
    /// no real tty is touched).
    fn setup_negotiation() -> NegotiationHarness {
        let writer = SharedWriter::default();
        let mut terminal = ProcessTerminal::with_writer(writer.clone());
        let input = Arc::new(Mutex::new(None));
        let input_sink = Arc::clone(&input);
        terminal.input_handler = Some(Box::new(move |data: &str| {
            *input_sink.lock().unwrap() = Some(data.to_string());
        }));
        terminal.query_and_enable_kitty_protocol();
        NegotiationHarness {
            terminal,
            writer,
            input,
        }
    }

    impl NegotiationHarness {
        /// Upstream `send(data)`: invoke the captured stdin `data` listener.
        fn send(&mut self, data: &str) {
            self.terminal.handle_stdin_data(data);
        }

        fn writes(&self) -> Vec<String> {
            self.writer.writes()
        }

        fn input(&self) -> Option<String> {
            self.input.lock().unwrap().clone()
        }

        /// Upstream `cleanup()`: `terminal.stop()` + reset the global Kitty
        /// protocol state; returns the captured writes for post-stop
        /// assertions.
        fn cleanup(mut self) -> Vec<String> {
            self.terminal.stop();
            set_kitty_protocol_active(false);
            self.writes()
        }
    }

    #[test]
    fn queries_kitty_mode_before_enabling_modify_other_keys_fallback() {
        let harness = setup_negotiation();
        let writes = harness.writes();
        assert_eq!(writes[0], "\x1b[>7u\x1b[?u\x1b[c");
        assert!(!writes.contains(&"\x1b[>4;2m".to_string()));
        assert!(!harness.terminal.kitty_protocol_active());
        harness.cleanup();
    }

    #[test]
    fn activates_kitty_mode_for_non_zero_negotiated_flags() {
        let mut harness = setup_negotiation();
        harness.send("\x1b[?7u");

        assert_eq!(harness.input(), None);
        assert!(harness.terminal.kitty_protocol_active());
        assert!(!harness.writes().contains(&"\x1b[>4;2m".to_string()));
        assert!(!harness.writes().contains(&"\x1b[>4;0m".to_string()));

        let writes = harness.cleanup();
        assert_eq!(
            writes
                .iter()
                .filter(|write| write.as_str() == "\x1b[<u")
                .count(),
            1
        );
        assert!(!writes.contains(&"\x1b[>4;0m".to_string()));
    }

    #[test]
    fn falls_back_to_modify_other_keys_for_zero_kitty_flags() {
        let mut harness = setup_negotiation();
        harness.send("\x1b[?0u");

        assert_eq!(harness.input(), None);
        assert!(!harness.terminal.kitty_protocol_active());
        assert_eq!(
            harness
                .writes()
                .iter()
                .filter(|write| write.as_str() == "\x1b[>4;2m")
                .count(),
            1
        );

        let writes = harness.cleanup();
        assert_eq!(
            writes
                .iter()
                .filter(|write| write.as_str() == "\x1b[>4;0m")
                .count(),
            1
        );
    }

    #[test]
    fn falls_back_to_modify_other_keys_for_device_attributes_without_kitty_flags() {
        let mut harness = setup_negotiation();
        harness.send("\x1b[?62;4;52c");

        assert_eq!(harness.input(), None);
        assert!(!harness.terminal.kitty_protocol_active());
        assert_eq!(
            harness
                .writes()
                .iter()
                .filter(|write| write.as_str() == "\x1b[>4;2m")
                .count(),
            1
        );
        harness.cleanup();
    }

    #[test]
    fn forwards_normal_input_while_waiting_for_kitty_response() {
        let mut harness = setup_negotiation();
        harness.send("a");

        assert_eq!(harness.input(), Some("a".to_string()));
        assert!(!harness.terminal.kitty_protocol_active());
        harness.cleanup();
    }

    #[test]
    fn tracks_split_kitty_confirmation() {
        let mut harness = setup_negotiation();
        harness.send("\x1b[?7");
        // Upstream `mock.timers.tick(10)` fires the StdinBuffer flush
        // (10ms timeout), routing the fragment into the negotiation buffer.
        harness
            .terminal
            .tick(Instant::now() + Duration::from_millis(20));

        assert_eq!(harness.input(), None);

        harness.send("u");

        assert!(harness.terminal.kitty_protocol_active());
        assert!(!harness.writes().contains(&"\x1b[>4;2m".to_string()));
        harness.cleanup();
    }

    #[test]
    fn replays_buffered_csi_prefix_input_when_it_is_not_a_kitty_response() {
        let mut harness = setup_negotiation();
        harness.send("\x1b[");
        // Upstream `mock.timers.tick(10)`: StdinBuffer flush routes "\x1b["
        // into the negotiation buffer (a response prefix) and schedules the
        // 150ms fragment flush.
        harness
            .terminal
            .tick(Instant::now() + Duration::from_millis(20));

        assert_eq!(harness.input(), None);

        // Upstream `mock.timers.tick(150)`: fragment flush forwards the
        // buffered prefix as plain input.
        harness
            .terminal
            .tick(Instant::now() + Duration::from_millis(200));

        assert_eq!(harness.input(), Some("\x1b[".to_string()));
        harness.cleanup();
    }

    // --- Dimensions (upstream describe "ProcessTerminal dimensions") ---

    #[test]
    fn falls_back_to_columns_and_lines_env_before_default_dimensions() {
        let previous_columns = std::env::var("COLUMNS").ok();
        let previous_lines = std::env::var("LINES").ok();
        std::env::set_var("COLUMNS", "123");
        std::env::set_var("LINES", "45");

        // Upstream sets `process.stdout.columns/rows` to undefined; here the
        // size query is injected to fail ("not a tty").
        let mut terminal = ProcessTerminal::with_writer(SharedWriter::default());
        terminal.query_size = || Err(io::Error::other("not a tty"));

        assert_eq!(terminal.columns(), 123);
        assert_eq!(terminal.rows(), 45);

        match previous_columns {
            Some(value) => std::env::set_var("COLUMNS", value),
            None => std::env::remove_var("COLUMNS"),
        }
        match previous_lines {
            Some(value) => std::env::set_var("LINES", value),
            None => std::env::remove_var("LINES"),
        }
    }

    // --- Port-specific regression tests for behavior the upstream suite only
    //     covers implicitly ---

    #[test]
    fn parse_keyboard_protocol_negotiation_sequence_matches_upstream_regexes() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?7u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags: 7 })
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?0u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags: 0 })
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?62;4;52c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
        // Empty digit run does not match `(\d+)u`.
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?u"),
            None
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?7"),
            None
        );
        assert_eq!(parse_keyboard_protocol_negotiation_sequence("a"), None);
    }

    #[test]
    fn cursor_and_clear_sequences_match_upstream_bytes() {
        let writer = SharedWriter::default();
        let mut terminal = ProcessTerminal::with_writer(writer.clone());

        terminal.move_by(3);
        terminal.move_by(-2);
        terminal.move_by(0);
        terminal.hide_cursor();
        terminal.show_cursor();
        terminal.clear_line();
        terminal.clear_from_cursor();
        terminal.clear_screen();
        terminal.set_title("hi");

        assert_eq!(
            writer.writes(),
            [
                "\x1b[3B",
                "\x1b[2A",
                "\x1b[?25l",
                "\x1b[?25h",
                "\x1b[K",
                "\x1b[J",
                "\x1b[2J\x1b[H",
                "\x1b]0;hi\x07",
            ]
        );
    }

    #[test]
    fn set_progress_writes_active_keepalive_and_stop_clears() {
        let writer = SharedWriter::default();
        let mut terminal = ProcessTerminal::with_writer(writer.clone());

        terminal.set_progress(true);
        assert_eq!(writer.writes(), [TERMINAL_PROGRESS_ACTIVE_SEQUENCE]);
        assert!(terminal.next_flush_deadline().is_some());

        // 1s keepalive rewrites the active sequence (upstream setInterval).
        terminal.tick(Instant::now() + Duration::from_millis(1100));
        assert_eq!(
            writer.writes(),
            [
                TERMINAL_PROGRESS_ACTIVE_SEQUENCE,
                TERMINAL_PROGRESS_ACTIVE_SEQUENCE
            ]
        );

        // stop() clears progress first (only because a keepalive was active),
        // then disables bracketed paste.
        terminal.stop();
        let writes = writer.writes();
        assert_eq!(writes[2], TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        assert_eq!(writes[3], "\x1b[?2004l");
    }

    #[test]
    fn set_progress_false_writes_clear_without_keepalive() {
        let writer = SharedWriter::default();
        let mut terminal = ProcessTerminal::with_writer(writer.clone());

        terminal.set_progress(false);
        assert_eq!(writer.writes(), [TERMINAL_PROGRESS_CLEAR_SEQUENCE]);
        assert_eq!(terminal.next_flush_deadline(), None);
    }

    #[tokio::test]
    async fn drain_input_disables_kitty_protocol_and_restores_input_handler() {
        let writer = SharedWriter::default();
        let mut terminal = ProcessTerminal::with_writer(writer.clone());
        let input = Arc::new(Mutex::new(None));
        let input_sink = Arc::clone(&input);
        terminal.input_handler = Some(Box::new(move |data: &str| {
            *input_sink.lock().unwrap() = Some(data.to_string());
        }));
        terminal.query_and_enable_kitty_protocol();
        terminal.handle_stdin_data("\x1b[?7u");
        assert!(terminal.kitty_protocol_active());

        terminal.drain_input(Some(100), Some(20)).await;

        assert!(!terminal.kitty_protocol_active());
        assert_eq!(
            writer
                .writes()
                .iter()
                .filter(|write| write.as_str() == "\x1b[<u")
                .count(),
            1
        );
        // The input handler is restored after draining.
        assert!(terminal.input_handler.is_some());
        set_kitty_protocol_active(false);
    }

    #[test]
    fn utc_timestamp_matches_known_dates() {
        assert_eq!(utc_timestamp(0), "1970-01-01_00-00-00");
        assert_eq!(utc_timestamp(86_400), "1970-01-02_00-00-00");
        // 2025-01-01T00:00:00Z
        assert_eq!(utc_timestamp(1_735_689_600), "2025-01-01_00-00-00");
        // 2026-08-05T02:17:56Z
        assert_eq!(utc_timestamp(1_785_896_276), "2026-08-05_02-17-56");
    }

    #[test]
    fn decode_utf8_incremental_holds_incomplete_tail_and_replaces_invalid() {
        let (text, rest) = decode_utf8_incremental("héllo".as_bytes());
        assert_eq!(text, "héllo");
        assert!(rest.is_empty());

        // Split a multi-byte character across chunks.
        let (text, rest) = decode_utf8_incremental(&[0xE4, 0xB8]);
        assert_eq!(text, "");
        assert_eq!(rest, vec![0xE4, 0xB8]);
        let (text, rest) = decode_utf8_incremental(&[&[0xE4, 0xB8][..], &[0x96][..]].concat());
        assert_eq!(text, "世");
        assert!(rest.is_empty());

        // Invalid bytes become U+FFFD.
        let (text, rest) = decode_utf8_incremental(&[0x61, 0xFF, 0x62]);
        assert_eq!(text, "a\u{FFFD}b");
        assert!(rest.is_empty());
    }
}
