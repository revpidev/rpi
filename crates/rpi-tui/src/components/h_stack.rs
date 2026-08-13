//! Port of `packages/tui/src/components/h-stack.ts` @ pi 4181f66.
//!
//! `HStack`: horizontal stack — a thin wrapper fixing `StackKind::Horizontal`,
//! plus the non-layout fallback render (`HStack.render`, h-stack.ts:12-43):
//! measure intrinsic widths, allocate across the safe width, re-render each
//! child at its allocated width and composite with `composite_tui_line`.
//! `align` only shifts children vertically (`center` offsets use `floor`;
//! rows landing on negative offsets are skipped).
//!
//! Intentional differences: none beyond those of `stack.rs` (which see).

use crate::components::stack::{
    allocate_stack_sizes, visible_stack_entries, Stack, StackChild, StackEntryOptions, StackOptions,
};
use crate::layout_node::{LayoutNode, LayoutViewport, StackAlign, StackKind};
use crate::tui::{composite_tui_line, lock_component, Component, SharedComponent};
use crate::utils::visible_width;

/// `HStack.render` (h-stack.ts:12-43), shared by `Stack::render` dispatch.
pub(crate) fn render_h_stack(stack: &Stack, width: usize) -> Vec<String> {
    let safe_width = width.max(1);
    let viewport = LayoutViewport {
        width: safe_width,
        height: usize::MAX,
    };
    let entries = visible_stack_entries(&stack.entries, viewport);
    if entries.is_empty() {
        return Vec::new();
    }

    let intrinsic_widths: Vec<f64> = entries
        .iter()
        .map(|entry| {
            let lines = lock_component(&entry.component).render(safe_width);
            lines
                .iter()
                .map(|line| visible_width(line))
                .max()
                .unwrap_or(0) as f64
        })
        .collect();
    let widths = allocate_stack_sizes(
        &entries,
        &intrinsic_widths,
        Some(safe_width as f64),
        stack.gap,
    );
    let rendered: Vec<Vec<String>> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            if widths[index] == 0.0 {
                Vec::new()
            } else {
                lock_component(&entry.component).render(widths[index] as usize)
            }
        })
        .collect();
    let height = rendered.iter().map(|lines| lines.len()).max().unwrap_or(0);
    let mut result = vec![String::new(); height];
    let mut x = 0i32;
    for (index, lines) in rendered.iter().enumerate() {
        // Allocated widths are floored by `clamp_size`, so the cast is exact.
        let child_width = widths[index] as usize;
        let offset: isize = match stack.align {
            StackAlign::Center => ((height - lines.len()) as f64 / 2.0).floor() as isize,
            StackAlign::End => (height - lines.len()) as isize,
            StackAlign::Stretch | StackAlign::Start => 0,
        };
        for (row, line) in lines.iter().enumerate() {
            let target = row as isize + offset;
            if target < 0 || target >= result.len() as isize {
                continue;
            }
            result[target as usize] = composite_tui_line(
                &result[target as usize],
                line,
                x,
                child_width as i32,
                safe_width as i32,
            );
        }
        x += (child_width + stack.gap as usize) as i32;
    }
    result
}

/// `HStack` (h-stack.ts:5-43).
pub struct HStack {
    stack: Stack,
}

impl HStack {
    pub fn new(children: Vec<StackChild>, options: StackOptions) -> Self {
        HStack {
            stack: Stack::new(StackKind::Horizontal, children, options),
        }
    }

    /// `Stack.addChild` (stack.ts:48-59).
    pub fn add_child(&mut self, component: SharedComponent, options: StackEntryOptions) {
        self.stack.add_child(component, options);
    }

    /// `Stack.removeChild` (stack.ts:61-65).
    pub fn remove_child(&mut self, component: &SharedComponent) {
        self.stack.remove_child(component);
    }

    /// `Stack.clear` (stack.ts:67-70).
    pub fn clear(&mut self) {
        self.stack.clear();
    }
}

impl Component for HStack {
    fn render(&self, width: usize) -> Vec<String> {
        self.stack.render(width)
    }

    fn invalidate(&mut self) {
        self.stack.invalidate();
    }

    fn shared_children(&self) -> Option<Vec<SharedComponent>> {
        self.stack.shared_children()
    }

    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        self.stack.layout_node()
    }
}
