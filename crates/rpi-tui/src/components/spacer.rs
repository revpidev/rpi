//! Port of `packages/tui/src/components/spacer.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

use std::cell::Cell;

use crate::tui::Component;

/// Spacer component that renders empty lines (upstream `Spacer`, spacer.ts:6).
pub struct Spacer {
    lines: usize,
    /// Monotonic content revision (V19-04 FR-E).
    revision: Cell<u64>,
    /// Process-unique instance id (V19-06).
    instance_id: u64,
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self {
            lines,
            revision: Cell::new(0),
            instance_id: crate::tui::next_cache_instance_id(),
        }
    }

    pub fn set_lines(&mut self, lines: usize) {
        if lines == self.lines {
            return;
        }
        self.lines = lines;
        self.revision.set(self.revision.get().wrapping_add(1));
    }
}

/// Upstream default constructor (`new Spacer()` renders one empty line).
impl Default for Spacer {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Component for Spacer {
    fn render(&self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }

    fn cache_revision(&self) -> Option<u64> {
        Some(self.revision.get())
    }

    fn cache_instance_id(&self) -> Option<u64> {
        Some(self.instance_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_that_many_empty_lines() {
        assert_eq!(Spacer::new(3).render(50), vec![String::new(); 3]);
        assert_eq!(Spacer::new(0).render(50), Vec::<String>::new());
    }

    #[test]
    fn set_lines_updates_render() {
        let mut spacer = Spacer::new(3);
        spacer.set_lines(1);
        assert_eq!(spacer.render(50).len(), 1);
    }

    #[test]
    fn default_renders_one_line() {
        assert_eq!(Spacer::default().render(10).len(), 1);
    }

    #[test]
    fn cache_revision_tracks_line_count() {
        let mut spacer = Spacer::new(3);
        let first = spacer.cache_revision().expect("Spacer is trusted");
        assert_eq!(spacer.cache_revision(), Some(first));
        spacer.set_lines(3);
        assert_eq!(spacer.cache_revision(), Some(first));
        spacer.set_lines(5);
        assert!(spacer.cache_revision().expect("revision") > first);
    }
}
