//! Dynamic border — port of
//! `packages/coding-agent/src/modes/interactive/components/dynamic-border.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The color function is required (`Box<dyn Fn(&str) -> String + Send +
//!   Sync>`); upstream defaults to the global `theme.fg("border")`, and the
//!   upstream comment warns that the global may be undefined under jiti —
//!   pir passes the color explicitly (bash-execution.ts:37-38).

use pir_tui::components::text::ColorFn;
use pir_tui::tui::Component;

/// `DynamicBorder` (dynamic-border.ts:11-24): a full-width `─` border line.
pub struct DynamicBorder {
    color: ColorFn,
}

impl DynamicBorder {
    pub fn new(color: ColorFn) -> Self {
        Self { color }
    }
}

impl Component for DynamicBorder {
    fn render(&self, width: usize) -> Vec<String> {
        vec![(self.color)(&"─".repeat(width.max(1)))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_full_width_border() {
        let border = DynamicBorder::new(Box::new(|s| format!("[{s}]")));
        assert_eq!(border.render(5), vec!["[─────]".to_string()]);
        // Width 0 still renders one glyph (Math.max(1, width)).
        assert_eq!(border.render(0), vec!["[─]".to_string()]);
    }
}
