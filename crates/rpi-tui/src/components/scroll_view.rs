//! Port of `packages/tui/src/components/scroll-view.ts` @ pi 4181f66.
//!
//! `ScrollView`: a fixed single-child container with scroll state, follow-end
//! tracking and a three-mode scrollbar (`hidden` / `auto` / `always`, where
//! `auto` is transient — shown on scroll activity, hidden after a delay).
//!
//! Intentional differences:
//! - The transient-scrollbar timer is NOT a thread/`setTimeout`: state holds
//!   `hide_deadline: Option<Instant>`; the host loop polls
//!   [`ScrollView::scrollbar_hide_deadline`] and drives [`ScrollView::tick`],
//!   which hides the scrollbar and fires the stored render callback when the
//!   deadline passes (deviation D-082; same explicit-deadline convention as
//!   T28). While `scrollbar_active` is set no deadline is armed, mirroring
//!   upstream clearing its timer.
//! - `axis` is not modeled: upstream only accepts `"vertical"` and throws
//!   otherwise, so the option does not exist here.
//! - Upstream overrides `addChild`/`removeChild`/`clear` to throw; here those
//!   methods simply do not exist — the child is fixed at construction.
//! - Scroll offsets are integers (`i64` in / `usize` stored, always truncated
//!   and clamped upstream); the non-finite-input fallbacks of `scrollTo` /
//!   `scrollBy` cannot occur and are dropped.
//! - `scrollbarHideDelayMs` is a `Duration` (upstream: `max(0, floor(ms))`,
//!   default 1000).
//! - State lives in a `RefCell` so every method takes `&self` (the
//!   `Component::render` contract); `Send` is preserved, matching the
//!   single-threaded render loop (same precedent as `components/text.rs`).

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::layout_node::{LayoutNode, ScrollLayoutNode, ScrollLayoutState};
use crate::tui::{lock_component, Component, RenderHandle, SharedComponent};

/// `ScrollViewScrollbar` (scroll-view.ts:4): `"hidden" | "auto" | "always"`.
///
/// Serde derives added for the `fullscreenScrollbar` settings key
/// (settings-manager.ts:1150-1159 @ 4181f66, T32); the wire format is the
/// lowercase string form matching upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollbarMode {
    #[default]
    Hidden,
    Auto,
    Always,
}

/// `ScrollViewOptions.follow` (scroll-view.ts:8): `"none" | "end"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Follow {
    #[default]
    None,
    End,
}

/// `ScrollViewOptions.overscroll` (scroll-view.ts:10): `"chain" | "contain"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overscroll {
    #[default]
    Chain,
    Contain,
}

/// `scrollbarStyle?: (text: string) => string` (scroll-view.ts:12).
pub type ScrollbarStyleFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Default `scrollbarStyle` (scroll-view.ts:45): grey background on the cell.
fn default_scrollbar_style(text: &str) -> String {
    format!("\x1b[100m{text}\x1b[49m")
}

/// `ScrollViewOptions` (scroll-view.ts:6-14), minus `axis` (see header note).
pub struct ScrollViewOptions {
    pub follow: Follow,
    pub primary: bool,
    pub overscroll: Overscroll,
    pub scrollbar: ScrollbarMode,
    pub scrollbar_style: Option<ScrollbarStyleFn>,
    pub scrollbar_hide_delay: Duration,
}

impl Default for ScrollViewOptions {
    fn default() -> Self {
        ScrollViewOptions {
            follow: Follow::None,
            primary: false,
            overscroll: Overscroll::Chain,
            scrollbar: ScrollbarMode::Hidden,
            scrollbar_style: None,
            // `scrollbarHideDelayMs ?? 1000` (scroll-view.ts:46).
            scrollbar_hide_delay: Duration::from_millis(1000),
        }
    }
}

/// Mutable scroll state (upstream private fields, scroll-view.ts:22-31).
struct ScrollViewState {
    current_scrollbar: ScrollbarMode,
    current_scroll_top: usize,
    content_height: usize,
    current_viewport_height: usize,
    following_end: bool,
    request_render_callback: Option<RenderHandle>,
    transient_scrollbar_visible: bool,
    scrollbar_active: bool,
    /// Replaces upstream's `scrollbarHideTimer` (scroll-view.ts:31); see the
    /// header note on the explicit-deadline timer model.
    hide_deadline: Option<Instant>,
}

/// `ScrollView` (scroll-view.ts:16-195).
pub struct ScrollView {
    child: SharedComponent,
    follow_end: bool,
    primary: bool,
    overscroll: Overscroll,
    /// Upstream `readonly scrollbarStyle` (scroll-view.ts:21).
    pub scrollbar_style: ScrollbarStyleFn,
    scrollbar_hide_delay: Duration,
    state: RefCell<ScrollViewState>,
}

impl ScrollView {
    /// Upstream constructor (scroll-view.ts:33-47).
    pub fn new(component: SharedComponent, options: ScrollViewOptions) -> Self {
        let follow_end = options.follow == Follow::End;
        ScrollView {
            child: component,
            follow_end,
            primary: options.primary,
            overscroll: options.overscroll,
            scrollbar_style: options
                .scrollbar_style
                .unwrap_or_else(|| Arc::new(default_scrollbar_style)),
            scrollbar_hide_delay: options.scrollbar_hide_delay,
            state: RefCell::new(ScrollViewState {
                current_scrollbar: options.scrollbar,
                current_scroll_top: 0,
                content_height: 0,
                current_viewport_height: 0,
                following_end: follow_end,
                request_render_callback: None,
                transient_scrollbar_visible: false,
                scrollbar_active: false,
                hide_deadline: None,
            }),
        }
    }

    /// The single fixed child (upstream private `child`, scroll-view.ts:17).
    pub fn child(&self) -> SharedComponent {
        self.child.clone()
    }

    /// `get scrollTop` (scroll-view.ts:49-51).
    pub fn scroll_top(&self) -> usize {
        self.state.borrow().current_scroll_top
    }

    /// `get isFollowingEnd` (scroll-view.ts:53-55).
    pub fn is_following_end(&self) -> bool {
        self.state.borrow().following_end
    }

    /// `get viewportHeight` (scroll-view.ts:57-59).
    pub fn viewport_height(&self) -> usize {
        self.state.borrow().current_viewport_height
    }

    /// `get scrollbar` (scroll-view.ts:61-63).
    pub fn scrollbar(&self) -> ScrollbarMode {
        self.state.borrow().current_scrollbar
    }

    /// Upstream `readonly primary` (scroll-view.ts:19).
    pub fn primary(&self) -> bool {
        self.primary
    }

    /// Upstream `readonly overscroll` (scroll-view.ts:20).
    pub fn overscroll(&self) -> Overscroll {
        self.overscroll
    }

    /// `get isScrollbarVisible` (scroll-view.ts:65-70).
    pub fn is_scrollbar_visible(&self) -> bool {
        let state = self.state.borrow();
        if state.current_scrollbar == ScrollbarMode::Always {
            return state.current_viewport_height > 0;
        }
        state.current_scrollbar == ScrollbarMode::Auto
            && state.content_height > state.current_viewport_height
            && state.transient_scrollbar_visible
    }

    /// `setScrollbar` (scroll-view.ts:72-78).
    pub fn set_scrollbar(&self, scrollbar: ScrollbarMode) {
        let callback = {
            let mut state = self.state.borrow_mut();
            if scrollbar == state.current_scrollbar {
                return;
            }
            state.current_scrollbar = scrollbar;
            if scrollbar != ScrollbarMode::Auto {
                Self::hide_transient_scrollbar_state(&mut state);
            } else if state.scrollbar_active {
                self.mark_scrollbar_activity_state(&mut state);
            }
            state.request_render_callback.clone()
        };
        if let Some(callback) = callback {
            callback.request_render();
        }
    }

    /// `getContentWidth` (scroll-view.ts:80-82): with `always` and more than
    /// one column, reserve the rightmost column for the scrollbar track.
    pub fn content_width(&self, width: usize) -> usize {
        if self.scrollbar() == ScrollbarMode::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    /// `markScrollbarActivity` (scroll-view.ts:84-98) on already-borrowed
    /// state. Shows the transient scrollbar and (re)arms the hide deadline —
    /// unless the scrollbar is being dragged (`scrollbar_active`), in which
    /// case no deadline is armed (upstream clears the timer and returns).
    fn mark_scrollbar_activity_state(&self, state: &mut ScrollViewState) {
        if state.current_scrollbar != ScrollbarMode::Auto
            || state.content_height <= state.current_viewport_height
        {
            return;
        }
        state.transient_scrollbar_visible = true;
        if state.scrollbar_active {
            state.hide_deadline = None;
            return;
        }
        state.hide_deadline = Some(Instant::now() + self.scrollbar_hide_delay);
    }

    /// `hideTransientScrollbar` (scroll-view.ts:100-105) on already-borrowed
    /// state.
    fn hide_transient_scrollbar_state(state: &mut ScrollViewState) {
        state.transient_scrollbar_visible = false;
        state.hide_deadline = None;
    }

    /// `setScrollbarActive` (scroll-view.ts:107-111).
    pub fn set_scrollbar_active(&self, active: bool) {
        let mut state = self.state.borrow_mut();
        if active == state.scrollbar_active {
            return;
        }
        state.scrollbar_active = active;
        self.mark_scrollbar_activity_state(&mut state);
    }

    /// `scrollTo` (scroll-view.ts:113-122).
    pub fn scroll_to(&self, scroll_top: i64) {
        let callback = {
            let mut state = self.state.borrow_mut();
            let max_scroll_top = max_scroll_top(&state) as i64;
            let next = scroll_top.clamp(0, max_scroll_top);
            if next == state.current_scroll_top as i64 {
                return;
            }
            state.current_scroll_top = next as usize;
            state.following_end = self.follow_end && next == max_scroll_top;
            self.mark_scrollbar_activity_state(&mut state);
            state.request_render_callback.clone()
        };
        if let Some(callback) = callback {
            callback.request_render();
        }
    }

    /// `scrollBy` (scroll-view.ts:124-138): returns the unconsumed delta
    /// (signed — negative when scrolling up past the start).
    pub fn scroll_by(&self, lines: i64) -> i64 {
        if lines == 0 {
            return 0;
        }
        let (callback, unconsumed) = {
            let mut state = self.state.borrow_mut();
            let max_scroll_top = max_scroll_top(&state) as i64;
            let start = if state.following_end {
                max_scroll_top
            } else {
                state.current_scroll_top as i64
            };
            let next = (start + lines).clamp(0, max_scroll_top);
            let moved = next - start;
            state.current_scroll_top = next as usize;
            state.following_end = self.follow_end && next == max_scroll_top;
            let callback = if moved != 0 {
                self.mark_scrollbar_activity_state(&mut state);
                state.request_render_callback.clone()
            } else {
                None
            };
            (callback, lines - moved)
        };
        if let Some(callback) = callback {
            callback.request_render();
        }
        // `requested - moved` (scroll-view.ts:137).
        unconsumed
    }

    /// `scrollToStart` (scroll-view.ts:140-150).
    pub fn scroll_to_start(&self) {
        let callback = {
            let mut state = self.state.borrow_mut();
            let target_following =
                self.follow_end && state.content_height <= state.current_viewport_height;
            let changed = state.current_scroll_top != 0 || state.following_end != target_following;
            state.current_scroll_top = 0;
            state.following_end = target_following;
            if changed {
                self.mark_scrollbar_activity_state(&mut state);
                state.request_render_callback.clone()
            } else {
                None
            }
        };
        if let Some(callback) = callback {
            callback.request_render();
        }
    }

    /// `scrollToEnd` (scroll-view.ts:152-161).
    pub fn scroll_to_end(&self) {
        let callback = {
            let mut state = self.state.borrow_mut();
            let next = max_scroll_top(&state);
            let changed =
                state.current_scroll_top != next || state.following_end != self.follow_end;
            state.current_scroll_top = next;
            state.following_end = self.follow_end;
            if changed {
                self.mark_scrollbar_activity_state(&mut state);
                state.request_render_callback.clone()
            } else {
                None
            }
        };
        if let Some(callback) = callback {
            callback.request_render();
        }
    }

    /// `updateLayout` (scroll-view.ts:163-172).
    pub fn update_layout(
        &self,
        content_height: usize,
        viewport_height: usize,
        request_render: RenderHandle,
    ) {
        let mut state = self.state.borrow_mut();
        state.content_height = content_height;
        state.current_viewport_height = viewport_height;
        state.request_render_callback = Some(request_render);
        let max_scroll_top = max_scroll_top(&state);
        if state.following_end {
            state.current_scroll_top = max_scroll_top;
        } else {
            state.current_scroll_top = state.current_scroll_top.min(max_scroll_top);
        }
        if self.follow_end && state.current_scroll_top == max_scroll_top {
            state.following_end = true;
        }
        if state.content_height <= state.current_viewport_height {
            Self::hide_transient_scrollbar_state(&mut state);
        }
    }

    /// Deadline by which the transient scrollbar hides (explicit-deadline
    /// replacement for upstream's `scrollbarHideTimer`). The host loop should
    /// call [`ScrollView::tick`] at or after this instant.
    pub fn scrollbar_hide_deadline(&self) -> Option<Instant> {
        self.state.borrow().hide_deadline
    }

    /// Drive the transient-scrollbar timer: once the deadline has passed,
    /// hide the scrollbar, clear the deadline and fire the stored render
    /// callback (upstream's `setTimeout` callback, scroll-view.ts:92-96).
    pub fn tick(&self, now: Instant) {
        let callback = {
            let mut state = self.state.borrow_mut();
            let Some(deadline) = state.hide_deadline else {
                return;
            };
            if now < deadline {
                return;
            }
            state.hide_deadline = None;
            state.transient_scrollbar_visible = false;
            state.request_render_callback.clone()
        };
        if let Some(callback) = callback {
            callback.request_render();
        }
    }
}

/// `Math.max(0, this.contentHeight - this.currentViewportHeight)`.
fn max_scroll_top(state: &ScrollViewState) -> usize {
    state
        .content_height
        .saturating_sub(state.current_viewport_height)
}

impl ScrollLayoutState for ScrollView {
    fn scroll_top(&self) -> usize {
        self.scroll_top()
    }

    fn primary(&self) -> bool {
        self.primary()
    }

    fn overscroll(&self) -> Overscroll {
        self.overscroll()
    }

    fn viewport_height(&self) -> usize {
        self.viewport_height()
    }

    fn content_width(&self, width: usize) -> usize {
        self.content_width(width)
    }

    fn update_layout(
        &self,
        content_height: usize,
        viewport_height: usize,
        request_render: RenderHandle,
    ) {
        self.update_layout(content_height, viewport_height, request_render);
    }
}

impl Component for ScrollView {
    /// `render` (scroll-view.ts:186-190): render the child at the content
    /// width; when a scrollbar column is reserved, pad every line with one
    /// trailing space to cover it.
    fn render(&self, width: usize) -> Vec<String> {
        let content_width = self.content_width(width);
        let lines = lock_component(&self.child).render(content_width);
        if content_width == width {
            lines
        } else {
            lines.into_iter().map(|line| format!("{line} ")).collect()
        }
    }

    fn invalidate(&mut self) {
        // `Container.invalidate` (tui.ts:229-233) for the single child.
        lock_component(&self.child).invalidate();
    }

    fn shared_children(&self) -> Option<Vec<SharedComponent>> {
        Some(vec![self.child.clone()])
    }

    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        // `[LAYOUT_NODE]()` (scroll-view.ts:192-194).
        Some(LayoutNode::Scroll(ScrollLayoutNode {
            component: self.child.clone(),
            state: self,
        }))
    }

    fn as_scroll_view(&self) -> Option<&ScrollView> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    //! Scroll semantics for `ScrollView` (scroll-view.ts): follow-end
    //! sequences, unconsumed `scrollBy` deltas, the three scrollbar modes,
    //! runtime mode switching, transient show/hide via deadline + `tick`,
    //! and the fallback `render` padding.

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::components::text::Text;
    use crate::tui::shared_component;

    fn text_view(options: ScrollViewOptions) -> ScrollView {
        ScrollView::new(shared_component(Text::new("line", 0, 0, None)), options)
    }

    /// RenderHandle counting invocations (upstream test spies on the
    /// `requestRender` callback passed to `updateLayout`).
    fn counting_handle(counter: &Arc<AtomicU64>) -> RenderHandle {
        let counter = counter.clone();
        RenderHandle::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn noop_handle() -> RenderHandle {
        RenderHandle::new(|| {})
    }

    // ---- scroll offsets & follow-end ----

    #[test]
    fn scroll_by_returns_unconsumed_signed_delta() {
        let view = text_view(ScrollViewOptions::default());
        view.update_layout(100, 10, noop_handle());
        assert_eq!(view.scroll_by(5), 0);
        assert_eq!(view.scroll_top(), 5);
        // At the bottom: scrolling further down consumes nothing.
        view.scroll_to(95);
        assert_eq!(view.scroll_top(), 90);
        assert_eq!(view.scroll_by(10), 10);
        // Scrolling up past the start returns a negative remainder.
        assert_eq!(view.scroll_by(-95), -5);
        assert_eq!(view.scroll_top(), 0);
    }

    #[test]
    fn follow_end_tracks_bottom_and_reports_delta_from_max() {
        let view = text_view(ScrollViewOptions {
            follow: Follow::End,
            ..ScrollViewOptions::default()
        });
        // Following from construction: updateLayout pins to the bottom.
        assert!(view.is_following_end());
        view.update_layout(100, 10, noop_handle());
        assert_eq!(view.scroll_top(), 90);
        assert!(view.is_following_end());
        // Scrolling up breaks the follow; the delta is fully consumed.
        assert_eq!(view.scroll_by(-5), 0);
        assert_eq!(view.scroll_top(), 85);
        assert!(!view.is_following_end());
        // Scrolling back to the exact bottom re-engages the follow.
        assert_eq!(view.scroll_by(5), 0);
        assert!(view.is_following_end());
        // While following, further downward scrolls start from maxScrollTop
        // and consume nothing.
        assert_eq!(view.scroll_by(5), 5);
        assert!(view.is_following_end());
        // Content growth keeps the follow pinned to the new bottom.
        view.update_layout(120, 10, noop_handle());
        assert_eq!(view.scroll_top(), 110);
        assert!(view.is_following_end());
    }

    #[test]
    fn scroll_to_start_and_end_update_following_state() {
        let view = text_view(ScrollViewOptions {
            follow: Follow::End,
            ..ScrollViewOptions::default()
        });
        view.update_layout(100, 10, noop_handle());
        view.scroll_to_start();
        assert_eq!(view.scroll_top(), 0);
        assert!(!view.is_following_end());
        view.scroll_to_end();
        assert_eq!(view.scroll_top(), 90);
        assert!(view.is_following_end());
        // Not following + shrinking content clamps scrollTop in updateLayout.
        view.scroll_to_start();
        view.scroll_by(40);
        view.update_layout(50, 10, noop_handle());
        assert_eq!(view.scroll_top(), 40);
        view.update_layout(30, 10, noop_handle());
        assert_eq!(view.scroll_top(), 20);
    }

    // ---- scrollbar modes ----

    #[test]
    fn scrollbar_visibility_follows_the_three_modes() {
        // Hidden: never visible.
        let hidden = text_view(ScrollViewOptions::default());
        hidden.update_layout(100, 10, noop_handle());
        hidden.scroll_by(5);
        assert!(!hidden.is_scrollbar_visible());

        // Always: visible whenever the viewport is non-zero.
        let always = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Always,
            ..ScrollViewOptions::default()
        });
        assert!(!always.is_scrollbar_visible());
        always.update_layout(5, 10, noop_handle());
        assert!(always.is_scrollbar_visible());

        // Auto: visible only with overflow AND recent scroll activity.
        let auto = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            ..ScrollViewOptions::default()
        });
        auto.update_layout(100, 10, noop_handle());
        assert!(!auto.is_scrollbar_visible());
        auto.scroll_by(1);
        assert!(auto.is_scrollbar_visible());
    }

    #[test]
    fn content_width_reserves_last_column_only_for_always() {
        let always = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Always,
            ..ScrollViewOptions::default()
        });
        assert_eq!(always.content_width(11), 10);
        // `width > 1` guard: a 1-column viewport stays 1.
        assert_eq!(always.content_width(1), 1);
        let auto = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            ..ScrollViewOptions::default()
        });
        assert_eq!(auto.content_width(11), 11);
    }

    #[test]
    fn set_scrollbar_switches_modes_at_runtime() {
        let view = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            ..ScrollViewOptions::default()
        });
        view.update_layout(100, 10, noop_handle());
        view.scroll_by(1);
        assert!(view.is_scrollbar_visible());
        assert_eq!(view.content_width(11), 11);
        // auto → always: transient state hidden, column reserved, visible.
        view.set_scrollbar(ScrollbarMode::Always);
        assert_eq!(view.content_width(11), 10);
        assert!(view.is_scrollbar_visible());
        // always → hidden: nothing visible.
        view.set_scrollbar(ScrollbarMode::Hidden);
        assert!(!view.is_scrollbar_visible());
        assert_eq!(view.content_width(11), 11);
    }

    // ---- transient scrollbar deadline + tick ----

    #[test]
    fn transient_scrollbar_hides_when_tick_passes_the_deadline() {
        let counter = Arc::new(AtomicU64::new(0));
        let view = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            scrollbar_hide_delay: Duration::from_millis(1000),
            ..ScrollViewOptions::default()
        });
        view.update_layout(100, 10, counting_handle(&counter));
        view.scroll_by(1);
        assert!(view.is_scrollbar_visible());
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        let deadline = view
            .scrollbar_hide_deadline()
            .unwrap_or_else(|| unreachable!());
        // Before the deadline: nothing changes.
        view.tick(deadline - Duration::from_millis(1));
        assert!(view.is_scrollbar_visible());
        assert!(view.scrollbar_hide_deadline().is_some());
        // At the deadline: hidden, deadline cleared, render requested.
        view.tick(deadline);
        assert!(!view.is_scrollbar_visible());
        assert!(view.scrollbar_hide_deadline().is_none());
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn scrollbar_active_suppresses_the_hide_deadline() {
        let view = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            ..ScrollViewOptions::default()
        });
        view.update_layout(100, 10, noop_handle());
        view.set_scrollbar_active(true);
        assert!(view.is_scrollbar_visible());
        assert!(view.scrollbar_hide_deadline().is_none());
        // Releasing the drag arms the deadline.
        view.set_scrollbar_active(false);
        assert!(view.scrollbar_hide_deadline().is_some());
    }

    #[test]
    fn transient_scrollbar_hides_when_content_fits_the_viewport() {
        let view = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            ..ScrollViewOptions::default()
        });
        view.update_layout(100, 10, noop_handle());
        view.scroll_by(1);
        assert!(view.is_scrollbar_visible());
        view.update_layout(10, 10, noop_handle());
        assert!(!view.is_scrollbar_visible());
        assert!(view.scrollbar_hide_deadline().is_none());
    }

    // ---- fallback render ----

    #[test]
    fn render_pads_a_trailing_space_over_the_reserved_scrollbar_column() {
        let always = text_view(ScrollViewOptions {
            scrollbar: ScrollbarMode::Always,
            ..ScrollViewOptions::default()
        });
        let lines = always.render(11);
        // Text pads to the content width (10); ScrollView adds one space.
        assert_eq!(lines, vec!["line       ".to_string()]);
        let hidden = text_view(ScrollViewOptions::default());
        assert_eq!(hidden.render(11), vec!["line       ".to_string()]);
    }

    #[test]
    fn layout_node_exposes_the_scroll_state_and_child() {
        let child = shared_component(Text::new("x", 0, 0, None));
        let view = ScrollView::new(child.clone(), ScrollViewOptions::default());
        view.update_layout(50, 10, noop_handle());
        let Some(LayoutNode::Scroll(node)) = view.layout_node() else {
            unreachable!()
        };
        assert!(crate::tui::same_component(&node.component, &child));
        assert_eq!(node.state.viewport_height(), 10);
        assert_eq!(node.state.scroll_top(), 0);
        assert!(!node.state.primary());
        assert_eq!(node.state.overscroll(), Overscroll::Chain);
        assert_eq!(node.state.content_width(11), 11);
        assert!(view.as_scroll_view().is_some());
    }
}
