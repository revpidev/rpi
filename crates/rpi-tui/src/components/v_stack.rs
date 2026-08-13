//! Port of `packages/tui/src/components/v-stack.ts` @ pi 4181f66.
//!
//! `VStack`: vertical stack — a thin wrapper fixing `StackKind::Vertical`,
//! plus the non-layout fallback render (`VStack.render`, v-stack.ts:10-30),
//! which carries the f24ab6e14 nested-minimum-size fix: render ALL visible
//! entries, clamp their line counts through `allocate_stack_sizes(.., None,
//! gap)` (basis/min/max applied), then truncate or pad each entry to its
//! clamped size with `gap` empty lines between entries.
//!
//! Intentional differences: none beyond those of `stack.rs` (which see).

use crate::components::stack::{
    allocate_stack_sizes, visible_stack_entries, Stack, StackChild, StackEntryOptions, StackOptions,
};
use crate::layout_node::{LayoutNode, LayoutViewport, StackKind};
use crate::tui::{lock_component, Component, SharedComponent};

/// `VStack.render` (v-stack.ts:10-30), shared by `Stack::render` dispatch.
pub(crate) fn render_v_stack(stack: &Stack, width: usize) -> Vec<String> {
    // `Number.MAX_SAFE_INTEGER` → `usize::MAX`: the unbounded viewport only
    // feeds `visible` predicates.
    let viewport = LayoutViewport {
        width: width.max(1),
        height: usize::MAX,
    };
    let entries = visible_stack_entries(&stack.entries, viewport);
    // Lock children one at a time; the stack's own lock is already held by
    // the caller, and child locks are released before the next is taken.
    let rendered: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| lock_component(&entry.component).render(viewport.width))
        .collect();
    let intrinsic_sizes: Vec<f64> = rendered.iter().map(|lines| lines.len() as f64).collect();
    let sizes = allocate_stack_sizes(&entries, &intrinsic_sizes, None, stack.gap);
    let mut lines: Vec<String> = Vec::new();
    for (index, child_lines) in rendered.iter().enumerate() {
        if index > 0 {
            for _ in 0..stack.gap as usize {
                lines.push(String::new());
            }
        }
        // Sizes come out of `clamp_size`, so they are already floored.
        let size = sizes[index] as usize;
        let taken = child_lines.len().min(size);
        lines.extend(child_lines[..taken].iter().cloned());
        for _ in taken..size {
            lines.push(String::new());
        }
    }
    lines
}

/// `VStack` (v-stack.ts:3-31).
pub struct VStack {
    stack: Stack,
}

impl VStack {
    pub fn new(children: Vec<StackChild>, options: StackOptions) -> Self {
        VStack {
            stack: Stack::new(StackKind::Vertical, children, options),
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

impl Component for VStack {
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
