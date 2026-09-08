//! Port of `packages/tui/src/components/box.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The render cache uses interior mutability (`RefCell`) because
//!   `Component::render` takes `&self`; the component stays `Send` (but not
//!   `Sync`), which matches the single-threaded render loop.
//! - The color callback is `Option<Box<dyn Fn(&str) -> String + Send + Sync>>`
//!   instead of the upstream TS `(text: string) => string` type.
//! - `StdBox` is an alias for `std::boxed::Box`; the component type itself is
//!   named `Box` (upstream spelling), which shadows the std one in this
//!   module.

use std::boxed::Box as StdBox;
use std::cell::RefCell;

use crate::components::text::ColorFn;
use crate::tui::{Component, TuiMouseEvent, TuiMouseHandlerResult};
use crate::utils::{apply_background_to_line, visible_width};

type RenderCache = Option<CacheEntry>;

struct CacheEntry {
    child_lines: Vec<String>,
    width: usize,
    bg_sample: Option<String>,
    lines: Vec<String>,
}

/// `mouseLayout` (box.ts:76 @ 9841914): per-child rendered heights at the
/// content width of the last `render`, reused by `handle_mouse` hit-testing.
struct MouseLayout {
    content_width: usize,
    heights: Vec<usize>,
}

/// Box component - a container that applies padding and background to all
/// children (upstream `Box`, box.ts:14).
pub struct Box {
    pub children: Vec<StdBox<dyn Component>>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<ColorFn>,

    // Cache for rendered output
    cache: RefCell<RenderCache>,
    mouse_layout: RefCell<Option<MouseLayout>>,
}

impl Box {
    pub fn new(padding_x: usize, padding_y: usize, bg_fn: Option<ColorFn>) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg_fn,
            cache: RefCell::new(None),
            mouse_layout: RefCell::new(None),
        }
    }

    pub fn add_child(&mut self, component: StdBox<dyn Component>) {
        self.children.push(component);
        self.invalidate_cache();
    }

    /// Remove by identity (upstream uses `indexOf` reference equality).
    pub fn remove_child(&mut self, component: &dyn Component) {
        let target = component as *const dyn Component as *const ();
        if let Some(index) = self
            .children
            .iter()
            .position(|child| &**child as *const dyn Component as *const () == target)
        {
            self.children.remove(index);
            self.invalidate_cache();
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
        self.invalidate_cache();
    }

    pub fn set_bg_fn(&mut self, bg_fn: Option<ColorFn>) {
        self.bg_fn = bg_fn;
        // Don't invalidate here - we'll detect bgFn changes by sampling output
    }

    fn invalidate_cache(&self) {
        *self.cache.borrow_mut() = None;
    }

    fn match_cache(
        &self,
        width: usize,
        child_lines: &[String],
        bg_sample: &Option<String>,
    ) -> bool {
        let cache = self.cache.borrow();
        let Some(cache) = cache.as_ref() else {
            return false;
        };
        cache.width == width
            && &cache.bg_sample == bg_sample
            && cache.child_lines.len() == child_lines.len()
            && cache
                .child_lines
                .iter()
                .zip(child_lines)
                .all(|(a, b)| a == b)
    }

    fn apply_bg(&self, line: &str, width: usize) -> String {
        let vis_len = visible_width(line);
        let pad_needed = width.saturating_sub(vis_len);
        let padded = format!("{line}{}", " ".repeat(pad_needed));

        if let Some(bg_fn) = &self.bg_fn {
            apply_background_to_line(&padded, width, bg_fn)
        } else {
            padded
        }
    }
}

impl Component for Box {
    fn render(&self, width: usize) -> Vec<String> {
        if self.children.is_empty() {
            return Vec::new();
        }

        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let left_pad = " ".repeat(self.padding_x);

        // `this.mouseLayout = { width: contentWidth, children:
        // mouseChildren }` (box.ts:128 @ 9841914) — heights of the children
        // as rendered at the content width, for `handle_mouse` hit-testing.
        let mut child_heights = Vec::with_capacity(self.children.len());
        let mut per_child_lines = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let lines = child.render(content_width);
            child_heights.push(lines.len());
            per_child_lines.push(lines);
        }
        *self.mouse_layout.borrow_mut() = Some(MouseLayout {
            content_width,
            heights: child_heights,
        });
        let mut child_lines: Vec<String> = Vec::new();
        for lines in per_child_lines {
            for line in lines {
                child_lines.push(format!("{left_pad}{line}"));
            }
        }

        if child_lines.is_empty() {
            return Vec::new();
        }

        // Check if bgFn output changed by sampling
        let bg_sample = self.bg_fn.as_ref().map(|bg_fn| bg_fn("test"));

        // Check cache validity
        if self.match_cache(width, &child_lines, &bg_sample) {
            if let Some(cache) = self.cache.borrow().as_ref() {
                return cache.lines.clone();
            }
        }

        // Apply background and padding
        let mut result: Vec<String> = Vec::new();

        // Top padding
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        // Content
        for line in &child_lines {
            result.push(self.apply_bg(line, width));
        }

        // Bottom padding
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        // Update cache
        let lines = result.clone();
        *self.cache.borrow_mut() = Some(CacheEntry {
            child_lines,
            width,
            bg_sample,
            lines,
        });

        result
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
        for child in &mut self.children {
            child.invalidate();
        }
    }

    /// `Box.prototype.handleMouse` (box.ts:75-97 @ 9841914, 71026970a):
    /// hit-test over the children at the content offset — a click on the
    /// padding (content x/y negative, or x at/beyond the content width)
    /// never reaches the children (`if (contentY < 0 || contentX < 0 ||
    /// contentX >= contentWidth) return undefined`, box.ts:79; rpi#38);
    /// in-content coordinates are translated to the child's local frame
    /// (`x` relative to the content origin, `y` relative to the child's
    /// row window, `width`/`height` the content width and the child's
    /// height). Heights come from the
    /// `mouseLayout` cache when the content width matches, else a
    /// measure-only render (not cached, like upstream). Upstream `Box`
    /// defines no `handleInput`, so no focus bubbling (see the `Container`
    /// note in `tui.rs`).
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
        let content_width = (event.width - (self.padding_x * 2) as isize).max(1) as usize;
        let content_y = event.y - self.padding_y as isize;
        let content_x = event.x - self.padding_x as isize;
        // Padding clicks stay with the screen-level dispatch (box.ts:79) —
        // without the x guard a click on the left/right padding column
        // still hit-tested into the child at a negative/overflowing local
        // x (children that ignore coordinates, e.g. MouseRegion wrappers,
        // fired on padding).
        if content_y < 0 || content_x < 0 || content_x >= content_width as isize {
            return None;
        }
        let cached_width_matches = self
            .mouse_layout
            .borrow()
            .as_ref()
            .is_some_and(|layout| layout.content_width == content_width);
        let heights = if cached_width_matches {
            self.mouse_layout
                .borrow()
                .as_ref()
                .map(|layout| layout.heights.clone())
                .unwrap_or_default()
        } else {
            self.children
                .iter()
                .map(|child| child.render(content_width).len())
                .collect::<Vec<_>>()
        };
        let mut child_y: isize = 0;
        for (child, child_height) in self.children.iter_mut().zip(heights) {
            let child_height = child_height as isize;
            if content_y >= child_y && content_y < child_y + child_height {
                let child_event = event.with_local(
                    content_x,
                    content_y - child_y,
                    content_width as isize,
                    child_height,
                );
                // Owned children cannot be dispatch targets (see the
                // `TuiMouseHandlerResult` note in `tui.rs`); the dispatcher
                // attaches this Box's shared root instead.
                return child.handle_mouse(&child_event);
            }
            child_y += child_height;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text::Text;

    #[test]
    fn renders_nothing_without_children() {
        let b = Box::new(1, 1, None);
        assert!(b.render(20).is_empty());
    }

    #[test]
    fn applies_padding_around_children() {
        let mut b = Box::new(1, 1, None);
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        let lines = b.render(10);

        // 1 top pad + 1 content + 1 bottom pad
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], " ".repeat(10));
        assert_eq!(lines[2], " ".repeat(10));
        assert_eq!(visible_width(&lines[1]), 10);
        // Child rendered at contentWidth 8 -> "hi      ", boxed -> " hi       "
        assert_eq!(lines[1], format!(" hi {}", " ".repeat(6)));
    }

    #[test]
    fn children_render_at_content_width() {
        let mut b = Box::new(2, 0, None);
        b.add_child(StdBox::new(Text::new("hello world", 0, 0, None)));
        let lines = b.render(20); // content width 20 - 4 = 16
        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 20);
    }

    #[test]
    fn applies_bg_fn_to_all_lines() {
        let mut b = Box::new(
            1,
            1,
            Some(StdBox::new(|line: &str| {
                format!("\x1b[48;5;1m{line}\x1b[0m")
            })),
        );
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        let lines = b.render(10);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.starts_with("\x1b[48;5;1m"));
            assert!(line.ends_with("\x1b[0m"));
            assert_eq!(visible_width(line), 10);
        }
    }

    #[test]
    fn add_child_invalidates_cache() {
        let mut b = Box::new(1, 1, None);
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        let before = b.render(10);
        assert_eq!(before.len(), 3);

        b.add_child(StdBox::new(Text::new("there", 0, 0, None)));
        let after = b.render(10);
        assert_ne!(before, after);
        assert_eq!(after.len(), 4); // 1 pad + 2 content + 1 pad
    }

    #[test]
    fn remove_child_uses_identity() {
        let mut b = Box::new(0, 0, None);
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        assert_eq!(b.render(10).len(), 1);

        // Removal is by reference identity (upstream `indexOf`), so the
        // reference must point at the exact boxed object.
        let child_ref = &*b.children[0] as *const dyn Component;
        // SAFETY: the child stays alive inside `b` for the duration of the
        // call; `remove_child` only compares the pointer address.
        unsafe { b.remove_child(&*child_ref) };
        assert!(b.render(10).is_empty());
    }

    #[test]
    fn clear_removes_all_children() {
        let mut b = Box::new(0, 0, None);
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        let before = b.render(10);
        b.clear();
        assert!(b.render(10).is_empty());
        assert_ne!(before, Vec::<String>::new());
    }

    #[test]
    fn set_bg_fn_is_detected_by_sampling() {
        let mut b = Box::new(0, 0, None);
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        let before = b.render(10);
        assert!(!before[0].contains('\x1b'));

        b.set_bg_fn(Some(StdBox::new(|line: &str| {
            format!("\x1b[48;5;1m{line}\x1b[0m")
        })));
        let after = b.render(10);
        assert!(after[0].starts_with("\x1b[48;5;1m"));
    }

    #[test]
    fn invalidate_cascades_to_children_and_rebuilds() {
        let mut b = Box::new(0, 0, None);
        b.add_child(StdBox::new(Text::new("hi", 0, 0, None)));
        let before = b.render(10);
        b.invalidate();
        assert_eq!(before, b.render(10));
    }

    // ------------------------------------------------------------------
    // V14-14: handleMouse (box.ts:75-97 @ 9841914, 71026970a)
    // ------------------------------------------------------------------

    use crate::tui::{
        TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
        TuiMouseHandlerResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Records clicks with the received local coordinates.
    struct ClickSpy {
        seen: Arc<AtomicUsize>,
        x: Arc<Mutex<isize>>,
        y: Arc<Mutex<isize>>,
    }

    impl Component for ClickSpy {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["row".to_string()]
        }

        fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
            if event.event_type == TuiMouseEventType::Click {
                self.seen.fetch_add(1, Ordering::SeqCst);
                *self.x.lock().unwrap() = event.x;
                *self.y.lock().unwrap() = event.y;
                return Some(TuiMouseHandlerResult::Event(TuiMouseEventResult {
                    handled: true,
                    ..Default::default()
                }));
            }
            None
        }
    }

    fn box_mouse_event(event_type: TuiMouseEventType, x: isize, y: isize) -> TuiMouseEvent {
        TuiMouseEvent {
            event_type,
            button: TuiMouseButton::Left,
            x,
            y,
            screen_x: x,
            screen_y: y,
            width: 20,
            height: 10,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: None,
            click_count: None,
        }
    }

    #[test]
    fn handle_mouse_translates_content_coordinates() {
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_x = Arc::new(Mutex::new(0));
        let seen_y = Arc::new(Mutex::new(0));
        let mut b = Box::new(2, 1, None);
        b.add_child(StdBox::new(Text::new("filler", 0, 0, None)));
        b.add_child(StdBox::new(ClickSpy {
            seen: Arc::clone(&seen),
            x: Arc::clone(&seen_x),
            y: Arc::clone(&seen_y),
        }));
        b.render(20);

        // Top padding row: no child at contentY = -1.
        assert!(b
            .handle_mouse(&box_mouse_event(TuiMouseEventType::Click, 5, 0))
            .is_none());
        // Filler row (contentY 0): filler has no handler.
        assert!(b
            .handle_mouse(&box_mouse_event(TuiMouseEventType::Click, 5, 1))
            .is_none());
        // Spy row (contentY 1 → local y 0; x shifted by padding 2).
        assert!(b
            .handle_mouse(&box_mouse_event(TuiMouseEventType::Click, 5, 2))
            .is_some());
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(*seen_x.lock().unwrap(), 3);
        assert_eq!(*seen_y.lock().unwrap(), 0);
    }

    /// rpi#38 (box.ts:79 @ 9841914): clicks on the padding columns never
    /// reach the children — the box returns `None` so the dispatcher keeps
    /// them for screen-level handling. Content width here is 20 − 2·2 = 16,
    /// so content x spans screen x 2..=17.
    #[test]
    fn handle_mouse_padding_columns_never_reach_children() {
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_x = Arc::new(Mutex::new(0));
        let seen_y = Arc::new(Mutex::new(0));
        let mut b = Box::new(2, 1, None);
        b.add_child(StdBox::new(ClickSpy {
            seen: Arc::clone(&seen),
            x: Arc::clone(&seen_x),
            y: Arc::clone(&seen_y),
        }));
        b.render(20);

        // The spy row (contentY 0 → screen y 1): clicks on the LEFT padding
        // columns (screen x 0/1 → contentX −2/−1) must not hit the child.
        for x in [0, 1] {
            assert!(
                b.handle_mouse(&box_mouse_event(TuiMouseEventType::Click, x, 1))
                    .is_none(),
                "left padding column x={x} must not reach children"
            );
        }
        // RIGHT padding columns (screen x 18/19 → contentX 16/17 ≥ 16).
        for x in [18, 19] {
            assert!(
                b.handle_mouse(&box_mouse_event(TuiMouseEventType::Click, x, 1))
                    .is_none(),
                "right padding column x={x} must not reach children"
            );
        }
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "padding clicks must not fire the child handler"
        );

        // In-content columns still dispatch (content x 0 and the last
        // column 15).
        for x in [2, 17] {
            assert!(
                b.handle_mouse(&box_mouse_event(TuiMouseEventType::Click, x, 1))
                    .is_some(),
                "in-content column x={x} must reach the child"
            );
        }
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }
}
