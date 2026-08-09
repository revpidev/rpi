//! Port of `packages/tui/src/components/spacer.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

use crate::tui::Component;

/// Spacer component that renders empty lines (upstream `Spacer`, spacer.ts:6).
pub struct Spacer {
    lines: usize,
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }

    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
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
}
