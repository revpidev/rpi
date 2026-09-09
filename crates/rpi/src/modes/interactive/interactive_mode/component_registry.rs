//! Interactive component registry — host side of the interactive custom UI
//! ABI (ADR-0024; v0.1.4 **C1**, V14-21).
//!
//! The guest protocol is frozen in `rpi_ext_host::interactive_ui` (V14-20
//! C0) and dispatched by `rpi-ext-host` (`ui.mountComponent` …). This module
//! is the native (L0) host runtime behind it:
//!
//! - [`ComponentRegistry`] — one active slot per extension owner (R-U1.6),
//!   globally unique handles, terminal-state rejection (R-U1.4/R-U6.4);
//! - [`MountState`] — queue + `Notify` (R-U1.2), visibility/focus state
//!   (R-U2/R-U4), frame buffer + limits (R-U3), dispose grace (R-U6.3);
//! - [`HostComponentProxy`] — the rpi-tui `Component`/`Focusable` that the
//!   overlay/editor region renders: it hands raw key bytes to the queue
//!   (R-U2.1), reports content width as `resize` (R-U3.3) and composites
//!   the last frame with `CURSOR_MARKER` / explicit cursor (R-U3.2);
//! - [`ComponentMountPoint`] — the mount surface, so tests can drive a fake
//!   overlay/editor and fake terminal size (R-U12.2).
//!
//! Out of C1 scope (per V14-21 §2.2): `ui.wakeComponent` (queue/`Notify`
//! infrastructure is in place, the method answers `unknownMethod` until C2),
//! `tick` delivery (C2), `ui.editExternal` (C3) and the full host-forced
//! dispose matrix (C3).
//!
//! [RPI-OWN]: the polling/line-frame mechanism has no upstream counterpart;
//! `Component::render(width)`/`handle_input(data)` mirror upstream
//! `ctx.ui.custom()` (`types.ts:197-212`, `tui.ts:111-135` @ `9841914`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rpi_ext_host::interactive_ui::{
    ComponentCursor, ComponentEvent, ComponentFrame, ComponentHandle, DisposeReason,
    InteractiveUiError, MountOptions, OverlayAnchor as WireAnchor, OverlayOptions as WireOverlay,
    SizeValue as WireSize, CURSOR_MARKER, DEFAULT_MAX_FRAME_ROWS, DEFAULT_MAX_LINE_BYTES,
};
use rpi_tui::keys::matches_key;
use rpi_tui::tui::{
    shared_component_from_boxed, Component, Focusable, OverlayAnchor, OverlayMargin,
    OverlayMarginSpec, OverlayOptions, SharedComponent, SizeValue, TuiInputListener,
    TuiInputListenerResult,
};
use rpi_tui::utils::{extract_segments, get_grapheme_cell_range, slice_by_column, visible_width};
use serde_json::Value;

/// Dispose grace (R-U6.3): how long the guest may take to submit a final
/// frame before the host force-unmounts (`dispose{timeout}`).
pub(crate) const DEFAULT_DISPOSE_GRACE: Duration = Duration::from_millis(500);

/// Event queue cap (R-U8.3 / design §3.7). `input` overflow is fail-visible
/// (unmount + structured error); `tick` may be dropped because it is only a
/// time signal.
pub(crate) const EVENT_QUEUE_CAPACITY: usize = 256;

const DIAGNOSTIC_CAPACITY: usize = 64;

/// One mount/unmount/error record (R-U12.1 diagnostics; also the test
/// injection point for log assertions, R-U12.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComponentDiagnostic {
    /// `mount` | `unmount` | `error` | `timeout` | `frameTooLarge` | `overflow`.
    pub kind: &'static str,
    pub owner: String,
    pub label: Option<String>,
    pub handle: u64,
    pub detail: Option<String>,
}

/// Overlay control surface (hide/focus/show) — the registry never touches
/// rpi-tui's `OverlayHandle` directly, so tests can drive a fake overlay
/// (R-U12.2). The live implementation wraps the real handle.
pub(crate) trait OverlayControl: Send + Sync {
    /// Temporarily hide/show the overlay (R-U4.3).
    fn set_hidden(&self, hidden: bool);
    /// Focus the overlay (R-U2.2 dialog restore).
    fn focus(&self);
    /// Permanently remove the overlay (R-U1.4 unmount).
    fn hide(&self);
}

/// The mount surface behind the registry. Implemented for the live TUI by
/// `UiMountPoint` (`interactive_mode.rs`) and by a fake in tests (R-U12.2).
pub(crate) trait ComponentMountPoint: Send + Sync {
    /// Mount `entry` as an overlay; returns the control surface (`None` only
    /// when the mount surface is already gone).
    fn mount_overlay(
        &self,
        entry: SharedComponent,
        options: OverlayOptions,
    ) -> Option<Arc<dyn OverlayControl>>;
    /// Mount `entry` into the editor region (show_selector semantics).
    fn show_editor_region(&self, entry: SharedComponent);
    /// Restore the editor when `entry` still owns the region.
    fn hide_editor_region(&self, entry: &SharedComponent);
    /// Request a redraw.
    fn request_render(&self);
    /// Register a raw-input listener (hidden-state keys, R-U2.3).
    fn add_input_listener(&self, listener: TuiInputListener) -> u64;
    /// Remove a raw-input listener by id.
    fn remove_input_listener(&self, id: u64);
    /// Terminal size `(columns, rows)`.
    fn terminal_size(&self) -> (usize, usize);
}

/// Last submitted frame + cursor (shared between the state and the proxy so
/// the proxy can keep rendering after the state is dropped).
struct FrameBuffer {
    lines: Mutex<Vec<String>>,
    cursor: Mutex<Option<ComponentCursor>>,
    cursor_enabled: bool,
}

/// Per-component runtime state. Self-closing: it owns every handle needed to
/// unmount (overlay/editor region, input listener, redraw) so the dispose
/// grace timer can run without a registry reference.
pub(crate) struct MountState {
    handle: ComponentHandle,
    owner: String,
    options: MountOptions,
    frame: Arc<FrameBuffer>,
    entry: Mutex<Option<SharedComponent>>,
    overlay: Mutex<Option<Arc<dyn OverlayControl>>>,
    queue: Mutex<VecDeque<ComponentEvent>>,
    notify: tokio::sync::Notify,
    terminal_error: Mutex<Option<InteractiveUiError>>,
    focused: AtomicBool,
    hidden: AtomicBool,
    /// Whether the component is currently shown in the TUI (overlay visible /
    /// editor region owned). Distinct from `hidden` while a host dialog owns
    /// the screen, when the actual mount change is deferred.
    mounted: AtomicBool,
    /// Whether the mount captures the keyboard on show (`nonCapturing`).
    capturing: bool,
    disposing: AtomicBool,
    closed: AtomicBool,
    dialog_open: AtomicBool,
    blur_for_dialog: AtomicBool,
    done: Mutex<Option<Value>>,
    last_resize: Mutex<Option<(usize, usize)>>,
    input_listener: Mutex<Option<u64>>,
    mount: Arc<dyn ComponentMountPoint>,
    diagnostics: Arc<Mutex<VecDeque<ComponentDiagnostic>>>,
    dispose_grace: Duration,
}

impl MountState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        handle: ComponentHandle,
        owner: &str,
        options: MountOptions,
        capturing: bool,
        frame: Arc<FrameBuffer>,
        mount: Arc<dyn ComponentMountPoint>,
        diagnostics: Arc<Mutex<VecDeque<ComponentDiagnostic>>>,
        dispose_grace: Duration,
    ) -> Self {
        Self {
            handle,
            owner: owner.to_owned(),
            options,
            frame,
            entry: Mutex::new(None),
            overlay: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            notify: tokio::sync::Notify::new(),
            terminal_error: Mutex::new(None),
            focused: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            mounted: AtomicBool::new(false),
            capturing,
            disposing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            dialog_open: AtomicBool::new(false),
            blur_for_dialog: AtomicBool::new(false),
            done: Mutex::new(None),
            last_resize: Mutex::new(None),
            input_listener: Mutex::new(None),
            mount,
            diagnostics,
            dispose_grace,
        }
    }

    fn log(&self, kind: &'static str, detail: Option<String>) {
        tracing::info!(
            target: "rpi::ext_ui",
            kind,
            owner = %self.owner,
            label = self.options.label.as_deref().unwrap_or(""),
            handle = self.handle.0,
            detail = detail.as_deref().unwrap_or(""),
            "interactive component"
        );
        let mut log = self
            .diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if log.len() >= DIAGNOSTIC_CAPACITY {
            log.pop_front();
        }
        log.push_back(ComponentDiagnostic {
            kind,
            owner: self.owner.clone(),
            label: self.options.label.clone(),
            handle: self.handle.0,
            detail,
        });
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Push an event, merging pending status-class events (R-U1.2 / §2.1).
    fn push_event(&self, event: ComponentEvent) {
        if self.is_closed() {
            return;
        }
        if self.disposing.load(Ordering::SeqCst) && !matches!(event, ComponentEvent::Dispose { .. })
        {
            return;
        }
        let status_class = matches!(
            event,
            ComponentEvent::Resize { .. }
                | ComponentEvent::Theme { .. }
                | ComponentEvent::Visibility { .. }
        );
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if status_class {
            if let Some(pending) = queue
                .iter_mut()
                .find(|pending| std::mem::discriminant(*pending) == std::mem::discriminant(&event))
            {
                *pending = event;
                drop(queue);
                self.notify.notify_one();
                return;
            }
        }
        if queue.len() >= EVENT_QUEUE_CAPACITY {
            drop(queue);
            if matches!(event, ComponentEvent::Tick) {
                // `tick` is a time signal, not a state transition; dropping it
                // under pressure cannot hide a bug (R-U1.2 / design §3.7).
                return;
            }
            self.fail(
                InteractiveUiError::invalid_request(format!(
                    "eventQueueOverflow: handle {} exceeded {EVENT_QUEUE_CAPACITY} pending events",
                    self.handle.0
                )),
                "overflow",
            );
            return;
        }
        queue.push_back(event);
        drop(queue);
        self.notify.notify_one();
    }

    /// Fail-visible unmount: record the structured error for a blocked
    /// `pollComponent`, log it, then force-unmount.
    fn fail(&self, error: InteractiveUiError, kind: &'static str) {
        if self.is_closed() {
            return;
        }
        let mut slot = self
            .terminal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(error.clone());
        }
        drop(slot);
        self.notify.notify_one();
        self.log(kind, Some(error.message.clone()));
        self.close(None);
    }

    /// Raw key bytes from the focused proxy (R-U2.1) — no normalization.
    /// Non-focused input is dropped here too (R-U2.4 defense in depth: the
    /// TUI already routes keys to the focused component only).
    fn push_input(&self, data: &str) {
        if self.is_closed()
            || self.disposing.load(Ordering::SeqCst)
            || self.hidden.load(Ordering::SeqCst)
            || !self.focused.load(Ordering::SeqCst)
        {
            return;
        }
        self.push_event(ComponentEvent::Input {
            data: data.to_owned(),
        });
    }

    /// `proxy.render(width)` hook: report the content width as `resize`
    /// (R-U3.3). Only emits on change, so repeated host renders do not spam
    /// the guest (R-U3.6).
    fn on_render_width(&self, width: usize) {
        if self.is_closed() || self.hidden.load(Ordering::SeqCst) {
            return;
        }
        let height = self.resolve_height();
        let mut last = self
            .last_resize
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last == Some((width, height)) {
            return;
        }
        *last = Some((width, height));
        drop(last);
        self.push_event(ComponentEvent::Resize { width, height });
    }

    /// Content-area height for `resize`: the configured `maxHeight` when
    /// present, otherwise the terminal rows (the available content area).
    fn resolve_height(&self) -> usize {
        let rows = self.mount.terminal_size().1.max(1);
        match self
            .options
            .overlay_options
            .as_ref()
            .and_then(|options| options.max_height.as_ref())
        {
            Some(WireSize::Absolute(value)) => (*value).max(1) as usize,
            Some(WireSize::Percent(percent)) => {
                (((rows as f64) * (*percent / 100.0)).floor() as i64).max(1) as usize
            }
            None => rows,
        }
    }

    fn pop_event(&self) -> Option<ComponentEvent> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
    }

    /// Block until an event is available, a terminal error is recorded or
    /// the component is force-unmounted (R-U1.2).
    async fn wait_event(&self) -> Result<ComponentEvent, InteractiveUiError> {
        loop {
            if let Some(error) = self
                .terminal_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                return Err(error);
            }
            if let Some(event) = self.pop_event() {
                return Ok(event);
            }
            // Register before the final re-check so a concurrent push cannot
            // be lost (`notify_one` stores a permit when no waiter is
            // registered).
            let notified = self.notify.notified();
            if let Some(error) = self
                .terminal_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                return Err(error);
            }
            if let Some(event) = self.pop_event() {
                return Ok(event);
            }
            notified.await;
        }
    }

    /// Guest-driven visibility (R-U4.3/R-U4.4). While a host dialog is open
    /// the actual mount is deferred to `dialog_closed` so the dialog is not
    /// replaced/focus-stolen.
    fn set_hidden(&self, hidden: bool) {
        if self.hidden.swap(hidden, Ordering::SeqCst) == hidden {
            return;
        }
        self.apply_visibility();
        self.push_event(ComponentEvent::Visibility { hidden });
    }

    /// Apply the requested visibility to the mount point (R-U4.3).
    /// Idempotent per `mounted`; skipped while a dialog owns the screen.
    fn apply_visibility(&self) {
        if self.is_closed() || self.dialog_open.load(Ordering::SeqCst) {
            return;
        }
        let hidden = self.hidden.load(Ordering::SeqCst);
        let mounted = self.mounted.load(Ordering::SeqCst);
        if let Some(overlay) = self
            .overlay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            if hidden && mounted {
                overlay.set_hidden(true);
                self.mounted.store(false, Ordering::SeqCst);
                self.focused.store(false, Ordering::SeqCst);
            } else if !hidden && !mounted {
                overlay.set_hidden(false);
                self.mounted.store(true, Ordering::SeqCst);
                self.focused.store(self.capturing, Ordering::SeqCst);
            }
        } else if let Some(entry) = self
            .entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            if hidden && mounted {
                self.mount.hide_editor_region(&entry);
                self.mounted.store(false, Ordering::SeqCst);
                self.focused.store(false, Ordering::SeqCst);
            } else if !hidden && !mounted {
                self.mount.show_editor_region(entry);
                self.mounted.store(true, Ordering::SeqCst);
                self.focused.store(true, Ordering::SeqCst);
            }
        }
        self.mount.request_render();
    }

    /// Host dialog opened (R-U2.2): the component loses focus and is told so.
    fn dialog_opened(&self) {
        if self.is_closed() || self.dialog_open.swap(true, Ordering::SeqCst) {
            return;
        }
        // Editor-region components lose their mount to the dialog (the region
        // is replaced); overlay components stay visible underneath it.
        if self
            .overlay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
        {
            self.mounted.store(false, Ordering::SeqCst);
        }
        if self.hidden.load(Ordering::SeqCst) || !self.focused.load(Ordering::SeqCst) {
            return;
        }
        self.blur_for_dialog.store(true, Ordering::SeqCst);
        self.focused.store(false, Ordering::SeqCst);
        self.push_event(ComponentEvent::Blur);
    }

    /// Host dialog closed (R-U2.2 / R-U10.2): restore focus only when the
    /// component was focused before AND is still visible. A component that
    /// became visible while the dialog was open is mounted now.
    fn dialog_closed(&self) {
        if !self.dialog_open.swap(false, Ordering::SeqCst) || self.is_closed() {
            return;
        }
        let restore_focus = self.blur_for_dialog.swap(false, Ordering::SeqCst);
        if self.hidden.load(Ordering::SeqCst) {
            // R-U10.2: an invisible component must not silently regain focus.
            return;
        }
        let was_mounted = self.mounted.load(Ordering::SeqCst);
        self.apply_visibility();
        if restore_focus {
            // Overlay mode: the overlay stayed mounted, refocus it. Editor
            // mode was remounted by `apply_visibility` above.
            if let Some(overlay) = self
                .overlay
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
            {
                overlay.focus();
            }
            self.mounted.store(true, Ordering::SeqCst);
            self.focused.store(true, Ordering::SeqCst);
            self.push_event(ComponentEvent::Focus);
        } else if !was_mounted && self.capturing {
            // It appeared while the dialog was open and now owns focus: the
            // guest should know.
            self.focused.store(true, Ordering::SeqCst);
            self.push_event(ComponentEvent::Focus);
        }
    }

    /// Update the last frame (R-U1.3). Validation happens in
    /// [`ComponentRegistry::render`]; this only stores + redraws.
    fn set_frame(&self, frame: &ComponentFrame) {
        {
            let mut lines = self
                .frame
                .lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *lines = frame.lines.clone();
        }
        {
            let mut cursor = self
                .frame
                .cursor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *cursor = frame.cursor;
        }
        self.mount.request_render();
    }

    /// Idempotent unmount (R-U1.4 / R-U6.4): hide the mount point, drop the
    /// hidden-key listener, wake a blocked poll and record the `done` value.
    fn close(&self, done: Option<Value>) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(id) = self
            .input_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            self.mount.remove_input_listener(id);
        }
        self.mounted.store(false, Ordering::SeqCst);
        if let Some(overlay) = self
            .overlay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            overlay.hide();
        } else if let Some(entry) = self
            .entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            self.mount.hide_editor_region(&entry);
        }
        if let Some(value) = done {
            *self
                .done
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(value);
        }
        self.focused.store(false, Ordering::SeqCst);
        let mut slot = self
            .terminal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(InteractiveUiError::unknown_handle(self.handle));
        }
        drop(slot);
        self.notify.notify_one();
        self.log("unmount", self.options.label.clone());
        self.mount.request_render();
    }

    /// Deliver `dispose{reason}` and force-unmount after the grace
    /// (R-U1.5 / R-U6.3). Idempotent per state.
    fn begin_dispose(self: &Arc<Self>, reason: DisposeReason) {
        if self.is_closed() || self.disposing.swap(true, Ordering::SeqCst) {
            return;
        }
        self.push_event(ComponentEvent::Dispose { reason });
        self.log("dispose", Some(reason.as_str().to_owned()));
        let weak = Arc::downgrade(self);
        let grace = self.dispose_grace;
        let force = move || {
            if let Some(state) = weak.upgrade() {
                if !state.is_closed() {
                    state.log("timeout", Some("disposeGraceExpired".to_owned()));
                    state.close(None);
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    tokio::time::sleep(grace).await;
                    force();
                });
            }
            Err(_) => {
                std::thread::spawn(move || {
                    std::thread::sleep(grace);
                    force();
                });
            }
        }
    }
}

/// rpi-tui component rendering the guest's line frame (design §2.6). Holds a
/// [`Weak`] state so the `MountState → entry → proxy → MountState` graph
/// cannot leak after unmount; the frame itself stays renderable through the
/// shared [`FrameBuffer`].
struct HostComponentProxy {
    state: Weak<MountState>,
    frame: Arc<FrameBuffer>,
}

impl Component for HostComponentProxy {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some(state) = self.state.upgrade() {
            state.on_render_width(width);
        }
        render_frame(&self.frame, width)
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(state) = self.state.upgrade() {
            state.push_input(data);
        }
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for HostComponentProxy {
    fn focused(&self) -> bool {
        self.state
            .upgrade()
            .map(|state| state.focused.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    fn set_focused(&mut self, focused: bool) {
        if let Some(state) = self.state.upgrade() {
            // The TUI is the authority on keyboard focus; the flag is used
            // only to decide blur/focus event delivery (R-U2.2). No events
            // are emitted here — the host dialog hooks own that contract.
            if focused || !state.dialog_open.load(Ordering::SeqCst) {
                state.focused.store(focused, Ordering::SeqCst);
            }
        }
    }
}

/// Compose the renderable lines: clip to width, strip/insert
/// `CURSOR_MARKER` and honour an explicit cursor (R-U3.1/R-U3.2).
fn render_frame(frame: &FrameBuffer, width: usize) -> Vec<String> {
    let lines = frame
        .lines
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let cursor = *frame
        .cursor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut out: Vec<String> = lines
        .iter()
        .map(|line| {
            let line = if frame.cursor_enabled {
                line.clone()
            } else {
                line.replace(CURSOR_MARKER, "")
            };
            if visible_width(&line) > width {
                slice_by_column(&line, 0, width, true)
            } else {
                line
            }
        })
        .collect();
    if !frame.cursor_enabled {
        return out;
    }
    if let Some(cursor) = cursor {
        // Explicit cursor wins over an embedded marker (R-U3.2).
        for line in out.iter_mut() {
            *line = line.replace(CURSOR_MARKER, "");
        }
        if let Some(line) = out.get_mut(cursor.row) {
            let mut col = cursor.col.min(visible_width(line));
            // Snap out of a wide grapheme so the split does not drop it.
            if let Some(range) = get_grapheme_cell_range(line, col) {
                if range.end > col {
                    col = range.start;
                }
            }
            let segments = extract_segments(line, col, col, usize::MAX - col, true);
            *line = format!("{}{}{}", segments.before, CURSOR_MARKER, segments.after);
        }
    }
    out
}

/// Validate a frame against the limits (R-U3.5): rows ≤ 5000, single line
/// ≤ 32 KiB, total (lines + `done` JSON) ≤ `maxFrameBytes`. The previous
/// frame is kept on failure (fail-visible).
fn validate_frame(
    frame: &ComponentFrame,
    max_frame_bytes: usize,
) -> Result<(), InteractiveUiError> {
    if frame.lines.len() > DEFAULT_MAX_FRAME_ROWS {
        return Err(InteractiveUiError::frame_too_large(format!(
            "rows {} > {DEFAULT_MAX_FRAME_ROWS}",
            frame.lines.len()
        )));
    }
    let mut total = 0usize;
    for (index, line) in frame.lines.iter().enumerate() {
        if line.len() > DEFAULT_MAX_LINE_BYTES {
            return Err(InteractiveUiError::frame_too_large(format!(
                "line {index} bytes {} > {DEFAULT_MAX_LINE_BYTES}",
                line.len()
            )));
        }
        total = total.saturating_add(line.len());
    }
    if let Some(done) = frame.done.value() {
        total = total.saturating_add(serde_json::to_string(done).map_or(0, |json| json.len()));
    }
    if total > max_frame_bytes {
        return Err(InteractiveUiError::frame_too_large(format!(
            "total bytes {total} > maxFrameBytes {max_frame_bytes}"
        )));
    }
    Ok(())
}

/// Mount-point geometry mapping (design §2.2 → rpi-tui `OverlayOptions`).
fn overlay_options(options: Option<&WireOverlay>, terminal_width: usize) -> OverlayOptions {
    let mut out = OverlayOptions::default();
    let Some(options) = options else {
        return out;
    };
    out.anchor = options.anchor.map(|anchor| match anchor {
        WireAnchor::Center => OverlayAnchor::Center,
        WireAnchor::TopLeft => OverlayAnchor::TopLeft,
        WireAnchor::TopRight => OverlayAnchor::TopRight,
        WireAnchor::BottomLeft => OverlayAnchor::BottomLeft,
        WireAnchor::BottomRight => OverlayAnchor::BottomRight,
        WireAnchor::TopCenter => OverlayAnchor::TopCenter,
        WireAnchor::BottomCenter => OverlayAnchor::BottomCenter,
        WireAnchor::LeftCenter => OverlayAnchor::LeftCenter,
        WireAnchor::RightCenter => OverlayAnchor::RightCenter,
    });
    out.width = options.width.map(to_tui_size);
    out.min_width = options.min_width.map(|value| match value {
        WireSize::Absolute(value) => value,
        WireSize::Percent(percent) => {
            (((terminal_width as f64) * (percent / 100.0)).floor() as i64) as i32
        }
    });
    out.max_height = options.max_height.map(to_tui_size);
    out.row = options.row.map(to_tui_size);
    out.col = options.col.map(to_tui_size);
    out.margin = options.margin.map(|margin| {
        OverlayMarginSpec::Edges(OverlayMargin {
            top: Some(margin.top),
            right: Some(margin.right),
            bottom: Some(margin.bottom),
            left: Some(margin.left),
        })
    });
    out.non_capturing = options.non_capturing;
    out
}

fn to_tui_size(value: WireSize) -> SizeValue {
    match value {
        WireSize::Absolute(value) => SizeValue::Absolute(value),
        WireSize::Percent(percent) => SizeValue::Percent(percent),
    }
}

/// The registry: one active component at a time (the per-extension slot is
/// subsumed — see the V14-21 §7 note), globally unique handles, fail-visible
/// diagnostics.
pub(crate) struct ComponentRegistry {
    next_handle: AtomicU64,
    active: Mutex<Option<Arc<MountState>>>,
    /// Handles that were closed through a registry call, for idempotent
    /// repeated `disposeComponent` (R-U6.4). Bounded.
    recently_closed: Mutex<VecDeque<u64>>,
    diagnostics: Arc<Mutex<VecDeque<ComponentDiagnostic>>>,
    dispose_grace: Duration,
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentRegistry {
    pub(crate) fn new() -> Self {
        Self::with_dispose_grace(DEFAULT_DISPOSE_GRACE)
    }

    /// Test/configuration hook for the dispose grace (R-U6.3 "可配").
    pub(crate) fn with_dispose_grace(dispose_grace: Duration) -> Self {
        Self {
            next_handle: AtomicU64::new(1),
            active: Mutex::new(None),
            recently_closed: Mutex::new(VecDeque::new()),
            diagnostics: Arc::new(Mutex::new(VecDeque::new())),
            dispose_grace,
        }
    }

    fn remember_closed(&self, handle: ComponentHandle) {
        let mut closed = self
            .recently_closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if closed.len() >= 32 {
            closed.pop_front();
        }
        closed.push_back(handle.0);
    }

    fn active_state(&self) -> Option<Arc<MountState>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Resolve `handle` for `owner`; closed/foreign/unknown handles are
    /// `invalidRequest`/`unknownHandle` (design §2.4, R-U8.2).
    fn lookup(
        &self,
        owner: &str,
        handle: ComponentHandle,
    ) -> Result<Arc<MountState>, InteractiveUiError> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match active.as_ref() {
            Some(state)
                if state.handle == handle
                    && state.owner == owner
                    && !state.closed.load(Ordering::SeqCst) =>
            {
                Ok(Arc::clone(state))
            }
            _ => Err(InteractiveUiError::unknown_handle(handle)),
        }
    }

    /// `ui.mountComponent` (FR-B / R-U1.1 / R-U4).
    pub(crate) fn mount(
        &self,
        owner: &str,
        options: MountOptions,
        mount: Arc<dyn ComponentMountPoint>,
    ) -> Result<ComponentHandle, InteractiveUiError> {
        {
            let active = self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(state) = active.as_ref() {
                if !state.closed.load(Ordering::SeqCst) {
                    // R-U1.6: never silently replace an active component.
                    return Err(InteractiveUiError::component_already_mounted());
                }
            }
        }
        let handle = ComponentHandle(self.next_handle.fetch_add(1, Ordering::SeqCst));
        let capturing = !options
            .overlay_options
            .as_ref()
            .is_some_and(|overlay| overlay.non_capturing);
        let frame = Arc::new(FrameBuffer {
            lines: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            cursor_enabled: options.cursor,
        });
        let state = Arc::new(MountState::new(
            handle,
            owner,
            options.clone(),
            capturing,
            Arc::clone(&frame),
            Arc::clone(&mount),
            Arc::clone(&self.diagnostics),
            self.dispose_grace,
        ));
        let proxy = HostComponentProxy {
            state: Arc::downgrade(&state),
            frame,
        };
        let entry = shared_component_from_boxed(Box::new(proxy));
        *state
            .entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&entry));

        // Hidden-state key whitelist (R-U2.3): a raw listener matches key
        // ids while hidden and consumes the match so it does not also land
        // in the editor.
        if !options.keys_when_hidden.is_empty() {
            let weak = Arc::downgrade(&state);
            let listener: TuiInputListener = Box::new(move |data: &str| {
                let state = weak.upgrade()?;
                if state.is_closed()
                    || state.disposing.load(Ordering::SeqCst)
                    || !state.hidden.load(Ordering::SeqCst)
                {
                    return None;
                }
                let matched = state
                    .options
                    .keys_when_hidden
                    .iter()
                    .any(|key| matches_key(data, key));
                if !matched {
                    return None;
                }
                state.push_event(ComponentEvent::Input {
                    data: data.to_owned(),
                });
                Some(TuiInputListenerResult {
                    consume: true,
                    data: None,
                })
            });
            let id = mount.add_input_listener(listener);
            *state
                .input_listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id);
        }

        if options.overlay {
            let terminal_width = mount.terminal_size().0;
            let geometry = overlay_options(options.overlay_options.as_ref(), terminal_width);
            if let Some(overlay) = mount.mount_overlay(Arc::clone(&entry), geometry) {
                *state
                    .overlay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(overlay);
            }
        } else {
            mount.show_editor_region(Arc::clone(&entry));
        }
        state.mounted.store(true, Ordering::SeqCst);
        state.focused.store(
            capturing && options.overlay || !options.overlay,
            Ordering::SeqCst,
        );
        state.log("mount", options.label.clone());
        *self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&state));
        mount.request_render();
        Ok(handle)
    }

    /// `ui.pollComponent` (FR-C / R-U1.2): blocking wait.
    ///
    /// Unlike the other methods this accepts a just-closed handle so the
    /// structured terminal error (queue overflow, dispose grace, `done`)
    /// reaches the guest instead of being flattened into `unknownHandle`.
    pub(crate) async fn poll(
        &self,
        owner: &str,
        handle: ComponentHandle,
    ) -> Result<ComponentEvent, InteractiveUiError> {
        let state = {
            let active = self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match active.as_ref() {
                Some(state) if state.handle == handle && state.owner == owner => Arc::clone(state),
                _ => return Err(InteractiveUiError::unknown_handle(handle)),
            }
        };
        state.wait_event().await
    }

    /// `ui.renderComponent` (FR-D / R-U1.3 / R-U3).
    pub(crate) fn render(
        &self,
        owner: &str,
        handle: ComponentHandle,
        frame: ComponentFrame,
    ) -> Result<(), InteractiveUiError> {
        let state = self.lookup(owner, handle)?;
        if let Err(error) = validate_frame(&frame, state.options.max_frame_bytes) {
            state.log("frameTooLarge", Some(error.message.clone()));
            return Err(error);
        }
        state.set_frame(&frame);
        if frame.is_done() {
            let done = frame.done.value().cloned();
            state.close(done);
            self.remember_closed(handle);
        }
        Ok(())
    }

    /// `ui.setComponentHidden` (FR-E / R-U4.3).
    pub(crate) fn set_hidden(
        &self,
        owner: &str,
        handle: ComponentHandle,
        hidden: bool,
    ) -> Result<(), InteractiveUiError> {
        let state = self.lookup(owner, handle)?;
        state.set_hidden(hidden);
        Ok(())
    }

    /// `ui.disposeComponent` (FR-F / R-U1.4 / R-U6.4): idempotent.
    pub(crate) fn dispose(
        &self,
        owner: &str,
        handle: ComponentHandle,
    ) -> Result<(), InteractiveUiError> {
        if let Ok(state) = self.lookup(owner, handle) {
            state.close(None);
            self.remember_closed(handle);
            return Ok(());
        }
        // Already closed: repeated dispose is a no-op (R-U6.4).
        let closed = self
            .recently_closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if closed.contains(&handle.0) {
            return Ok(());
        }
        Err(InteractiveUiError::unknown_handle(handle))
    }

    /// Host-forced dispose (R-U1.5): deliver `dispose{reason}`, then
    /// force-unmount after the grace. Wired to the full host matrix in C3;
    /// C1 exercises it through tests and `dispose_active`.
    pub(crate) fn dispose_active(&self, reason: DisposeReason) -> Option<ComponentHandle> {
        let state = self.active_state()?;
        if state.is_closed() {
            return None;
        }
        state.begin_dispose(reason);
        Some(state.handle)
    }

    /// Theme change broadcast (R-U3.4).
    pub(crate) fn notify_theme(&self, theme: Value) {
        if let Some(state) = self.active_state() {
            state.push_event(ComponentEvent::Theme { theme });
        }
    }

    /// Host dialog opened/closed hooks (R-U2.2 / R-U10).
    pub(crate) fn dialog_opened(&self) {
        if let Some(state) = self.active_state() {
            state.dialog_opened();
        }
    }

    pub(crate) fn dialog_closed(&self) {
        if let Some(state) = self.active_state() {
            state.dialog_closed();
        }
    }

    /// The active handle, if any (diagnostics / lifecycle assertions).
    #[cfg(test)]
    pub(crate) fn active_handle(&self) -> Option<ComponentHandle> {
        self.active_state()
            .filter(|state| !state.is_closed())
            .map(|state| state.handle)
    }

    /// `(active, closed)` component counts for leak assertions.
    #[cfg(test)]
    pub(crate) fn live_counts(&self) -> (usize, usize) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match active.as_ref() {
            Some(state) if !state.is_closed() => (1, 0),
            _ => (0, 0),
        }
    }

    /// Diagnostics snapshot (R-U12.1/R-U12.2 log assertions).
    #[cfg(test)]
    pub(crate) fn diagnostics(&self) -> Vec<ComponentDiagnostic> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Test injection point (R-U12.2): push a synthetic event into the
    /// active component's queue.
    #[cfg(test)]
    pub(crate) fn inject_event(&self, event: ComponentEvent) -> bool {
        match self.active_state() {
            Some(state) if !state.is_closed() => {
                state.push_event(event);
                true
            }
            _ => false,
        }
    }

    /// Test injection point (R-U12.2): the active component's entry.
    #[cfg(test)]
    pub(crate) fn active_entry(&self) -> Option<SharedComponent> {
        self.active_state()
            .and_then(|state| state.entry.lock().ok().and_then(|entry| entry.clone()))
    }
}

// ============================================================================
// Tests (V14-21 §4.1–§4.7; R-U12.2 fake mount point / fake overlay)
// ============================================================================

#[cfg(test)]
mod interactive_component {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use rpi_ext_host::interactive_ui::InteractiveUiErrorKind;

    use super::*;

    /// Overlay geometry captured from the mount call (rpi-tui `OverlayOptions`
    /// is not `Clone`/`Debug` because of its `visible` callback).
    #[derive(Clone, Debug, PartialEq)]
    struct OverlayRecord {
        anchor: Option<OverlayAnchor>,
        width: Option<SizeValue>,
        min_width: Option<i32>,
        max_height: Option<SizeValue>,
        row: Option<SizeValue>,
        col: Option<SizeValue>,
        margin: Option<OverlayMarginSpec>,
        non_capturing: bool,
    }

    impl OverlayRecord {
        fn from(options: &OverlayOptions) -> Self {
            Self {
                anchor: options.anchor,
                width: options.width,
                min_width: options.min_width,
                max_height: options.max_height,
                row: options.row,
                col: options.col,
                margin: options.margin,
                non_capturing: options.non_capturing,
            }
        }
    }

    #[derive(Default)]
    struct FakeOverlayState {
        hidden: Mutex<bool>,
        focused: AtomicBool,
        live: AtomicBool,
        hides: AtomicUsize,
        set_hidden_calls: AtomicUsize,
        focus_calls: AtomicUsize,
    }

    struct FakeOverlayControl {
        state: Arc<FakeOverlayState>,
    }

    impl OverlayControl for FakeOverlayControl {
        fn set_hidden(&self, hidden: bool) {
            self.state.set_hidden_calls.fetch_add(1, Ordering::SeqCst);
            *self.state.hidden.lock().unwrap() = hidden;
            if hidden {
                self.state.focused.store(false, Ordering::SeqCst);
            }
        }

        fn focus(&self) {
            self.state.focus_calls.fetch_add(1, Ordering::SeqCst);
            self.state.focused.store(true, Ordering::SeqCst);
        }

        fn hide(&self) {
            self.state.hides.fetch_add(1, Ordering::SeqCst);
            self.state.live.store(false, Ordering::SeqCst);
            self.state.focused.store(false, Ordering::SeqCst);
        }
    }

    /// Deterministic mount surface (R-U12.2): records geometry, owns a fake
    /// overlay, the editor region and the raw-input listener table.
    #[derive(Default)]
    struct FakeMountPoint {
        overlays: Mutex<Vec<OverlayRecord>>,
        overlay_states: Mutex<Vec<Arc<FakeOverlayState>>>,
        editor: Mutex<Option<SharedComponent>>,
        editor_shows: AtomicUsize,
        editor_hides: AtomicUsize,
        render_requests: AtomicUsize,
        listeners: Mutex<HashMap<u64, TuiInputListener>>,
        next_listener: AtomicU64,
        size: Mutex<(usize, usize)>,
    }

    impl FakeMountPoint {
        fn with_size(width: usize, height: usize) -> Self {
            let fake = Self::default();
            *fake.size.lock().unwrap() = (width, height);
            fake
        }

        fn last_overlay(&self) -> Arc<FakeOverlayState> {
            self.overlay_states
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("an overlay was mounted")
        }

        /// Deliver raw input through the registered listeners (hidden-key
        /// path). Returns whether a listener consumed it.
        fn deliver_input(&self, data: &str) -> bool {
            let mut listeners = self.listeners.lock().unwrap();
            for listener in listeners.values_mut() {
                if let Some(result) = listener(data) {
                    if result.consume {
                        return true;
                    }
                }
            }
            false
        }

        fn listener_count(&self) -> usize {
            self.listeners.lock().unwrap().len()
        }
    }

    impl ComponentMountPoint for FakeMountPoint {
        fn mount_overlay(
            &self,
            _entry: SharedComponent,
            options: OverlayOptions,
        ) -> Option<Arc<dyn OverlayControl>> {
            self.overlays
                .lock()
                .unwrap()
                .push(OverlayRecord::from(&options));
            let state = Arc::new(FakeOverlayState::default());
            state.live.store(true, Ordering::SeqCst);
            self.overlay_states.lock().unwrap().push(Arc::clone(&state));
            Some(Arc::new(FakeOverlayControl { state }))
        }

        fn show_editor_region(&self, entry: SharedComponent) {
            self.editor_shows.fetch_add(1, Ordering::SeqCst);
            *self.editor.lock().unwrap() = Some(entry);
        }

        fn hide_editor_region(&self, entry: &SharedComponent) {
            let mut editor = self.editor.lock().unwrap();
            if editor
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, entry))
            {
                self.editor_hides.fetch_add(1, Ordering::SeqCst);
                *editor = None;
            }
        }

        fn request_render(&self) {
            self.render_requests.fetch_add(1, Ordering::SeqCst);
        }

        fn add_input_listener(&self, listener: TuiInputListener) -> u64 {
            let id = self.next_listener.fetch_add(1, Ordering::SeqCst) + 1;
            self.listeners.lock().unwrap().insert(id, listener);
            id
        }

        fn remove_input_listener(&self, id: u64) {
            self.listeners.lock().unwrap().remove(&id);
        }

        fn terminal_size(&self) -> (usize, usize) {
            *self.size.lock().unwrap()
        }
    }

    fn mount_overlay(
        registry: &ComponentRegistry,
        mount: &Arc<FakeMountPoint>,
        options: MountOptions,
    ) -> ComponentHandle {
        registry
            .mount(
                "ext",
                options,
                Arc::clone(mount) as Arc<dyn ComponentMountPoint>,
            )
            .expect("mount")
    }

    fn entry_of(registry: &ComponentRegistry) -> SharedComponent {
        registry.active_entry().expect("active entry")
    }

    fn input_entry(registry: &ComponentRegistry, data: &str) {
        let entry = entry_of(registry);
        entry.lock().unwrap().handle_input(data);
    }

    fn render_entry(registry: &ComponentRegistry, width: usize) -> Vec<String> {
        let entry = entry_of(registry);
        let guard = entry.lock().unwrap();
        guard.render(width)
    }

    fn drain_events(registry: &ComponentRegistry, handle: ComponentHandle) -> Vec<ComponentEvent> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        let mut events = Vec::new();
        runtime.block_on(async {
            while let Ok(Ok(event)) = tokio::time::timeout(
                std::time::Duration::from_millis(5),
                registry.poll("ext", handle),
            )
            .await
            {
                events.push(event);
            }
        });
        events
    }

    // -- §4.1 registry lifecycle -------------------------------------------

    #[test]
    fn component_registry_lifecycle_handles_and_rejection() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let first = mount_overlay(&registry, &mount, MountOptions::default());
        // R-U1.6: a second mount is rejected, never a silent replace.
        let error = registry
            .mount(
                "ext",
                MountOptions::default(),
                Arc::clone(&mount) as Arc<dyn ComponentMountPoint>,
            )
            .expect_err("already mounted");
        assert_eq!(error.kind, InteractiveUiErrorKind::Call);
        assert!(error.message.contains("componentAlreadyMounted"));

        // done → the handle is terminal.
        registry
            .render(
                "ext",
                first,
                ComponentFrame::lines(vec!["x".to_owned()]).with_done(Value::Null),
            )
            .expect("done");
        assert_eq!(registry.active_handle(), None);
        for method in ["poll", "render", "hidden"] {
            let error = match method {
                "poll" => drain_poll_error(&registry, "ext", first),
                "render" => registry
                    .render("ext", first, ComponentFrame::lines(vec![]))
                    .expect_err("terminal"),
                _ => registry
                    .set_hidden("ext", first, true)
                    .expect_err("terminal"),
            };
            assert_eq!(
                error.kind,
                InteractiveUiErrorKind::InvalidRequest,
                "{method}"
            );
            assert!(error.message.contains("unknownHandle"), "{method}");
        }
        // Repeated dispose is an idempotent no-op (R-U6.4).
        registry.dispose("ext", first).expect("idempotent dispose");
        registry.dispose("ext", first).expect("idempotent dispose");

        // A new mount gets a fresh, globally unique handle.
        let second = mount_overlay(&registry, &mount, MountOptions::default());
        assert_ne!(first, second);
        registry.dispose("ext", second).expect("dispose");
        assert_eq!(registry.live_counts(), (0, 0));
        // The foreign-owner path is rejected too (R-U8.2).
        let third = mount_overlay(&registry, &mount, MountOptions::default());
        assert!(drain_poll_error(&registry, "other", third)
            .message
            .contains("unknownHandle"));
        registry.dispose("ext", third).expect("dispose");
    }

    fn drain_poll_error(
        registry: &ComponentRegistry,
        owner: &str,
        handle: ComponentHandle,
    ) -> InteractiveUiError {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                registry.poll(owner, handle),
            )
            .await
            .expect("terminal poll returns immediately")
            .expect_err("terminal handle")
        })
    }

    // -- §4.2 mount points --------------------------------------------------

    #[test]
    fn component_registry_overlay_options_mapping() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(120, 40));
        let options = MountOptions {
            overlay: true,
            overlay_options: Some(WireOverlay {
                anchor: Some(WireAnchor::BottomCenter),
                width: Some(WireSize::Percent(100.0)),
                min_width: Some(WireSize::Absolute(40)),
                max_height: Some(WireSize::Percent(50.0)),
                row: Some(WireSize::Percent(25.0)),
                col: Some(WireSize::Absolute(3)),
                margin: Some(rpi_ext_host::interactive_ui::Margin {
                    left: 1,
                    right: 2,
                    bottom: 3,
                    top: 4,
                }),
                non_capturing: true,
            }),
            tick_ms: 0,
            keys_when_hidden: Vec::new(),
            cursor: true,
            max_frame_bytes: 1024,
            label: Some("ask_user_question".to_owned()),
        };
        let handle = mount_overlay(&registry, &mount, options);
        let records = mount.overlays.lock().unwrap().clone();
        assert_eq!(
            records[0],
            OverlayRecord {
                anchor: Some(OverlayAnchor::BottomCenter),
                width: Some(SizeValue::Percent(100.0)),
                min_width: Some(40),
                max_height: Some(SizeValue::Percent(50.0)),
                row: Some(SizeValue::Percent(25.0)),
                col: Some(SizeValue::Absolute(3)),
                margin: Some(OverlayMarginSpec::Edges(OverlayMargin {
                    top: Some(4),
                    right: Some(2),
                    bottom: Some(3),
                    left: Some(1),
                })),
                non_capturing: true,
            }
        );
        // R-U2.4: nonCapturing does not steal focus → no input delivery.
        input_entry(&registry, "a");
        assert!(drain_events(&registry, handle).is_empty());
        registry.dispose("ext", handle).expect("dispose");
    }

    #[test]
    fn component_registry_editor_region_mount_and_restore() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let options = MountOptions {
            overlay: false,
            ..MountOptions::default()
        };
        let handle = mount_overlay(&registry, &mount, options);
        assert_eq!(mount.editor_shows.load(Ordering::SeqCst), 1);
        assert!(mount.editor.lock().unwrap().is_some());
        // Editor-region mounts are focused.
        input_entry(&registry, "x");
        let events = drain_events(&registry, handle);
        assert!(events
            .iter()
            .any(|event| matches!(event, ComponentEvent::Input { data } if data == "x")));
        registry.dispose("ext", handle).expect("dispose");
        assert_eq!(mount.editor_hides.load(Ordering::SeqCst), 1);
        assert!(mount.editor.lock().unwrap().is_none());
        assert_eq!(registry.live_counts(), (0, 0));
    }

    #[test]
    fn component_registry_first_event_is_resize_with_content_width() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(120, 40));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        // The overlay renderer calls proxy.render(content_width).
        assert!(render_entry(&registry, 64).is_empty());
        let events = drain_events(&registry, handle);
        assert_eq!(
            events,
            vec![ComponentEvent::Resize {
                width: 64,
                height: 40,
            }]
        );
        // Repeated renders at the same width do not spam the guest (R-U3.6).
        assert!(render_entry(&registry, 64).is_empty());
        assert!(drain_events(&registry, handle).is_empty());
        registry.dispose("ext", handle).expect("dispose");
    }

    // -- §4.3 polling and merging ------------------------------------------

    #[tokio::test]
    async fn component_registry_status_events_merge_event_class_does_not() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry.inject_event(ComponentEvent::Resize {
            width: 80,
            height: 24,
        });
        registry.inject_event(ComponentEvent::Resize {
            width: 100,
            height: 30,
        });
        registry.inject_event(ComponentEvent::Theme {
            theme: serde_json::json!({"name": "a"}),
        });
        registry.inject_event(ComponentEvent::Theme {
            theme: serde_json::json!({"name": "b"}),
        });
        registry.inject_event(ComponentEvent::Theme {
            theme: serde_json::json!({"name": "c"}),
        });
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Resize {
                width: 100,
                height: 30
            }
        );
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Theme {
                theme: serde_json::json!({"name": "c"})
            }
        );

        // input → resize → input keeps its order; resize never merges across
        // an event-class event.
        input_entry(&registry, "a");
        registry.inject_event(ComponentEvent::Resize {
            width: 50,
            height: 10,
        });
        input_entry(&registry, "b");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Input {
                data: "a".to_owned()
            }
        );
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Resize {
                width: 50,
                height: 10
            }
        );
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Input {
                data: "b".to_owned()
            }
        );
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_poll_parks_without_blocking_the_runtime() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        // No events: the poll must stay pending (and not occupy a worker —
        // this is a current-thread runtime, so a blocked worker would
        // deadlock the timeout below).
        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(30),
            registry.poll("ext", handle),
        )
        .await;
        assert!(pending.is_err(), "poll returned without an event");
        // An event wakes it.
        registry.inject_event(ComponentEvent::Tick);
        let event = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            registry.poll("ext", handle),
        )
        .await
        .expect("woken")
        .expect("event");
        assert_eq!(event, ComponentEvent::Tick);
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_input_overflow_is_fail_visible() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        for index in 0..EVENT_QUEUE_CAPACITY {
            input_entry(&registry, &format!("{index}"));
        }
        assert_eq!(registry.live_counts(), (1, 0));
        // The 257th input overflows: unmount + structured error, no silent
        // key loss.
        input_entry(&registry, "overflow");
        let error = registry.poll("ext", handle).await.expect_err("overflow");
        assert_eq!(error.kind, InteractiveUiErrorKind::InvalidRequest);
        assert!(error.message.contains("eventQueueOverflow"), "{error}");
        assert_eq!(registry.live_counts(), (0, 0));
        let overlay = mount.last_overlay();
        assert!(!overlay.live.load(Ordering::SeqCst), "overlay unmounted");
        assert_eq!(mount.listener_count(), 0);
        let kinds: Vec<&str> = registry
            .diagnostics()
            .iter()
            .map(|entry| entry.kind)
            .collect();
        assert!(kinds.contains(&"overflow"), "{kinds:?}");
    }

    #[tokio::test]
    async fn component_registry_tick_overflow_is_dropped_not_fatal() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        for index in 0..EVENT_QUEUE_CAPACITY {
            input_entry(&registry, &format!("{index}"));
        }
        registry.inject_event(ComponentEvent::Tick);
        assert_eq!(registry.live_counts(), (1, 0));
        let first = registry.poll("ext", handle).await.expect("event");
        assert!(matches!(first, ComponentEvent::Input { .. }));
        registry.dispose("ext", handle).expect("dispose");
    }

    // -- §4.4 input / focus / hidden / dialog matrix -----------------------

    #[tokio::test]
    async fn component_registry_input_focus_and_dialog_matrix() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());

        // focused + visible: raw CSI/SS3/Kitty bytes and Esc pass through
        // verbatim (R-U2.1/R-U2.5).
        for data in ["\u{1b}[B", "\u{1b}OA", "\u{1b}[97;5u", "\u{1b}"] {
            input_entry(&registry, data);
        }
        for data in ["\u{1b}[B", "\u{1b}OA", "\u{1b}[97;5u", "\u{1b}"] {
            assert_eq!(
                registry.poll("ext", handle).await.unwrap(),
                ComponentEvent::Input {
                    data: data.to_owned()
                }
            );
        }

        // Host dialog opens → blur; keys do not reach the component.
        registry.dialog_opened();
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Blur
        );
        input_entry(&registry, "x");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                registry.poll("ext", handle)
            )
            .await
            .is_err(),
            "blurred component must not receive keys"
        );
        // Dialog closes → focus restored.
        registry.dialog_closed();
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Focus
        );
        assert_eq!(mount.last_overlay().focus_calls.load(Ordering::SeqCst), 1);
        input_entry(&registry, "y");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Input {
                data: "y".to_owned()
            }
        );
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_hidden_state_and_keys_when_hidden() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let options = MountOptions {
            keys_when_hidden: vec!["ctrl+]".to_owned()],
            ..MountOptions::default()
        };
        let handle = mount_overlay(&registry, &mount, options);
        assert_eq!(mount.listener_count(), 1);

        registry.set_hidden("ext", handle, true).expect("hide");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Visibility { hidden: true }
        );
        // Whitelisted key routes; anything else is ignored and not consumed.
        assert!(!mount.deliver_input("a"));
        assert!(mount.deliver_input("\u{1d}"));
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Input {
                data: "\u{1d}".to_owned()
            }
        );
        // While hidden the proxy is not focused: ordinary keys dropped.
        input_entry(&registry, "z");
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            registry.poll("ext", handle)
        )
        .await
        .is_err());
        registry.set_hidden("ext", handle, false).expect("show");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Visibility { hidden: false }
        );
        registry.dispose("ext", handle).expect("dispose");
        assert_eq!(mount.listener_count(), 0);
    }

    #[tokio::test]
    async fn component_registry_dialog_close_does_not_focus_hidden_component() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry.dialog_opened();
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Blur
        );
        registry.set_hidden("ext", handle, true).expect("hide");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Visibility { hidden: true }
        );
        registry.dialog_closed();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                registry.poll("ext", handle)
            )
            .await
            .is_err(),
            "hidden component must not regain focus (R-U10.2)"
        );
        assert_eq!(mount.last_overlay().focus_calls.load(Ordering::SeqCst), 0);
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_editor_region_dialog_remounts_on_close() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let options = MountOptions {
            overlay: false,
            ..MountOptions::default()
        };
        let handle = mount_overlay(&registry, &mount, options);
        assert_eq!(mount.editor_shows.load(Ordering::SeqCst), 1);
        registry.dialog_opened();
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Blur
        );
        // The dialog replaced the editor region (the fake keeps the entry;
        // the registry treats it as unmounted until the dialog closes).
        registry.dialog_closed();
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Focus
        );
        // Region remounted (the dialog had taken it).
        assert_eq!(mount.editor_shows.load(Ordering::SeqCst), 2);
        assert!(mount.editor.lock().unwrap().is_some());
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_unhide_during_dialog_mounts_on_close() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry.set_hidden("ext", handle, true).expect("hide");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Visibility { hidden: true }
        );
        // Hidden when the dialog opened: no blur.
        registry.dialog_opened();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            registry.poll("ext", handle)
        )
        .await
        .is_err());
        // Shown again while the dialog is open: the mount is deferred.
        registry.set_hidden("ext", handle, false).expect("show");
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Visibility { hidden: false }
        );
        registry.dialog_closed();
        // Mounted now, and capturing components gain focus.
        let overlay = mount.last_overlay();
        assert!(!*overlay.hidden.lock().unwrap());
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Focus
        );
        registry.dispose("ext", handle).expect("dispose");
    }

    // -- §4.5 rendering and cursor -----------------------------------------

    #[test]
    fn component_registry_frame_render_cursor_and_clipping() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry
            .render(
                "ext",
                handle,
                ComponentFrame {
                    lines: vec!["abcdef".to_owned(), "a\u{1b}[31mred\u{1b}[39mz".to_owned()],
                    cursor: Some(ComponentCursor { row: 0, col: 3 }),
                    done: rpi_ext_host::interactive_ui::DoneValue::Absent,
                },
            )
            .expect("render");
        // Explicit cursor wins: marker inserted after "abc".
        let lines = render_entry(&registry, 80);
        assert_eq!(lines[0], format!("abc{CURSOR_MARKER}def"));
        assert_eq!(lines[1], "a\u{1b}[31mred\u{1b}[39mz");
        // Clipping to the overlay width keeps ANSI styling; the zero-width
        // cursor marker stays at its visual column inside the clipped line.
        let clipped = render_entry(&registry, 4);
        assert_eq!(clipped[0], format!("abc{CURSOR_MARKER}d"));
        assert_eq!(visible_width(&clipped[1]), 4);
        registry.dispose("ext", handle).expect("dispose");
    }

    #[test]
    fn component_registry_frame_marker_passthrough_and_cursor_disabled() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        // Default: a guest-embedded marker survives for the TUI to strip.
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry
            .render(
                "ext",
                handle,
                ComponentFrame::lines(vec![format!("ab{CURSOR_MARKER}cd")]),
            )
            .expect("render");
        assert_eq!(
            render_entry(&registry, 80)[0],
            format!("ab{CURSOR_MARKER}cd")
        );
        // Explicit cursor strips the embedded marker first.
        registry
            .render(
                "ext",
                handle,
                ComponentFrame {
                    lines: vec![format!("ab{CURSOR_MARKER}cd")],
                    cursor: Some(ComponentCursor { row: 0, col: 1 }),
                    done: rpi_ext_host::interactive_ui::DoneValue::Absent,
                },
            )
            .expect("render");
        assert_eq!(
            render_entry(&registry, 80)[0],
            format!("a{CURSOR_MARKER}bcd")
        );
        registry.dispose("ext", handle).expect("dispose");

        // `cursor: false` strips every marker and never injects one.
        let options = MountOptions {
            cursor: false,
            ..MountOptions::default()
        };
        let handle = mount_overlay(&registry, &mount, options);
        registry
            .render(
                "ext",
                handle,
                ComponentFrame {
                    lines: vec![format!("ab{CURSOR_MARKER}cd")],
                    cursor: Some(ComponentCursor { row: 0, col: 2 }),
                    done: rpi_ext_host::interactive_ui::DoneValue::Absent,
                },
            )
            .expect("render");
        assert_eq!(render_entry(&registry, 80)[0], "abcd");
        registry.dispose("ext", handle).expect("dispose");
    }

    #[test]
    fn component_registry_frame_limits_keep_previous_frame() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry
            .render(
                "ext",
                handle,
                ComponentFrame::lines(vec!["good".to_owned()]),
            )
            .expect("first frame");

        // rows > 5000
        let rows = ComponentFrame::lines(vec![String::new(); DEFAULT_MAX_FRAME_ROWS + 1]);
        let error = registry.render("ext", handle, rows).expect_err("rows");
        assert!(error.message.contains("frameTooLarge"), "{error}");
        // single line > 32 KiB
        let long = ComponentFrame::lines(vec!["x".repeat(DEFAULT_MAX_LINE_BYTES + 1)]);
        assert!(registry.render("ext", handle, long).is_err());
        // total > maxFrameBytes
        let options = MountOptions {
            max_frame_bytes: 8,
            ..MountOptions::default()
        };
        registry.dispose("ext", handle).expect("dispose");
        let handle = mount_overlay(&registry, &mount, options);
        registry
            .render("ext", handle, ComponentFrame::lines(vec!["ok".to_owned()]))
            .expect("small frame");
        let error = registry
            .render(
                "ext",
                handle,
                ComponentFrame::lines(vec!["0123456789".to_owned()]),
            )
            .expect_err("total");
        assert!(error.message.contains("maxFrameBytes"), "{error}");
        // `done` payload counts toward the total (R-U3.5).
        let error = registry
            .render(
                "ext",
                handle,
                ComponentFrame::lines(vec!["ok".to_owned()])
                    .with_done(serde_json::json!({"answers": "0123456789"})),
            )
            .expect_err("done bytes");
        assert!(error.message.contains("maxFrameBytes"), "{error}");
        // fail-visible: the previous frame is still what renders.
        assert_eq!(render_entry(&registry, 80), vec!["ok".to_owned()]);
        let kinds: Vec<&str> = registry
            .diagnostics()
            .iter()
            .map(|entry| entry.kind)
            .collect();
        assert!(kinds.contains(&"frameTooLarge"), "{kinds:?}");
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_done_null_is_recorded() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry
            .render(
                "ext",
                handle,
                ComponentFrame::lines(vec!["x".to_owned()]).with_done(Value::Null),
            )
            .expect("done null");
        let state = registry.active_state().expect("closed state retained");
        assert!(state.is_closed());
        assert_eq!(*state.done.lock().unwrap(), Some(Value::Null));
    }

    // -- §4.6 dispose grace -------------------------------------------------

    #[tokio::test]
    async fn component_registry_dispose_grace_guest_final_frame() {
        let registry = ComponentRegistry::with_dispose_grace(std::time::Duration::from_millis(40));
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        assert_eq!(
            registry.dispose_active(DisposeReason::ToolAbort),
            Some(handle)
        );
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Dispose {
                reason: DisposeReason::ToolAbort
            }
        );
        // The guest submits its final frame inside the grace window.
        registry
            .render(
                "ext",
                handle,
                ComponentFrame::lines(vec!["bye".to_owned()]).with_done(serde_json::json!({})),
            )
            .expect("final frame");
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let kinds: Vec<&str> = registry
            .diagnostics()
            .iter()
            .map(|entry| entry.kind)
            .collect();
        assert!(!kinds.contains(&"timeout"), "{kinds:?}");
        assert_eq!(registry.live_counts(), (0, 0));
        assert!(!mount.last_overlay().live.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn component_registry_dispose_grace_force_unmount_on_timeout() {
        let registry = ComponentRegistry::with_dispose_grace(std::time::Duration::from_millis(30));
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        assert_eq!(
            registry.dispose_active(DisposeReason::SessionShutdown),
            Some(handle)
        );
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Dispose {
                reason: DisposeReason::SessionShutdown
            }
        );
        // The guest never answers: the host force-unmounts after the grace.
        tokio::time::sleep(std::time::Duration::from_millis(90)).await;
        assert_eq!(registry.live_counts(), (0, 0));
        assert!(!mount.last_overlay().live.load(Ordering::SeqCst));
        assert_eq!(mount.listener_count(), 0);
        let diagnostics = registry.diagnostics();
        let timeout = diagnostics
            .iter()
            .find(|entry| entry.kind == "timeout")
            .expect("timeout diagnostic");
        assert_eq!(timeout.owner, "ext");
        assert_eq!(timeout.handle, handle.0);
        assert_eq!(timeout.detail.as_deref(), Some("disposeGraceExpired"));
        // A blocked poll wakes with a structured error, not a panic.
        let error = registry.poll("ext", handle).await.expect_err("closed");
        assert!(error.message.contains("unknownHandle"), "{error}");
    }

    #[test]
    fn component_registry_default_dispose_grace_is_500ms() {
        assert_eq!(DEFAULT_DISPOSE_GRACE, std::time::Duration::from_millis(500));
    }

    // -- §4.7 diagnostics ---------------------------------------------------

    #[tokio::test]
    async fn component_registry_diagnostics_carry_owner_label_and_reason() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let options = MountOptions {
            label: Some("ask_user_question".to_owned()),
            ..MountOptions::default()
        };
        let handle = mount_overlay(&registry, &mount, options);
        registry.dispose("ext", handle).expect("dispose");
        let diagnostics = registry.diagnostics();
        let mount_entry = diagnostics
            .iter()
            .find(|entry| entry.kind == "mount")
            .expect("mount diagnostic");
        assert_eq!(mount_entry.owner, "ext");
        assert_eq!(mount_entry.label.as_deref(), Some("ask_user_question"));
        assert_eq!(mount_entry.handle, handle.0);
        let unmount_entry = diagnostics
            .iter()
            .find(|entry| entry.kind == "unmount")
            .expect("unmount diagnostic");
        assert_eq!(unmount_entry.owner, "ext");
        assert_eq!(unmount_entry.label.as_deref(), Some("ask_user_question"));
    }

    // -- §4.5 golden screen frames (byte-exact, independent naming) --------

    /// Write/compare a golden under `crates/rpi/tests/snapshots/` (same
    /// convention as `interactive/snapshots.rs`; regenerate with
    /// `RPI_UPDATE_SNAPSHOTS=1`).
    fn assert_golden(name: &str, lines: &[String]) {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
        let path = dir.join(format!("{name}.snap"));
        let actual = format!("{}\n", lines.join("\n"));
        if std::env::var_os("RPI_UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(&dir).expect("create snapshot dir");
            std::fs::write(&path, &actual).expect("write snapshot");
            return;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert_eq!(actual, expected, "golden {name} drifted");
    }

    /// A JSONL frame script drives the proxy through the mount/render loop;
    /// the composed lines (ANSI, `CURSOR_MARKER`, clipping) are compared
    /// byte-exactly. The "screen frame" here is the overlay component's
    /// render contract — the layer the rpi-tui compositor consumes
    /// (`composite_tui_line` pads/positions it).
    #[test]
    fn interactive_component_golden_frame_script() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        // {lines, cursor?, done?} — the C0 ComponentFrame wire shape.
        let script = [
            r#"{"lines":["header","","body line"]}"#,
            r#"{"lines":["abc\u001b_pi:c\u0007def","\u001b[31mred\u001b[39m"]}"#,
            r#"{"lines":["abcdefghij"],"cursor":{"row":0,"col":5}}"#,
            r#"{"lines":["clipped-to-four","\u001b[1mbold\u001b[22m tail"]}"#,
        ];
        let mut golden = Vec::new();
        for (index, json) in script.iter().enumerate() {
            let frame = ComponentFrame::from_json(json).expect("frame json");
            registry.render("ext", handle, frame).expect("render");
            let width = if index == 3 { 4 } else { 40 };
            golden.push(format!("# frame {index} (width {width})"));
            golden.extend(render_entry(&registry, width));
        }
        assert_golden("interactive_component_frame_script", &golden);
        registry.dispose("ext", handle).expect("dispose");
    }

    #[tokio::test]
    async fn component_registry_theme_broadcast_reaches_active_component() {
        let registry = ComponentRegistry::new();
        let mount = Arc::new(FakeMountPoint::with_size(80, 24));
        let handle = mount_overlay(&registry, &mount, MountOptions::default());
        registry.notify_theme(serde_json::json!({"name": "light"}));
        assert_eq!(
            registry.poll("ext", handle).await.unwrap(),
            ComponentEvent::Theme {
                theme: serde_json::json!({"name": "light"})
            }
        );
        registry.dispose("ext", handle).expect("dispose");
    }
}
