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
use crate::tui::Component;
use crate::utils::{apply_background_to_line, visible_width};

type RenderCache = Option<CacheEntry>;

struct CacheEntry {
    child_lines: Vec<String>,
    width: usize,
    bg_sample: Option<String>,
    lines: Vec<String>,
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
}

impl Box {
    pub fn new(padding_x: usize, padding_y: usize, bg_fn: Option<ColorFn>) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg_fn,
            cache: RefCell::new(None),
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

        // Render all children
        let mut child_lines: Vec<String> = Vec::new();
        for child in &self.children {
            let lines = child.render(content_width);
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
}
