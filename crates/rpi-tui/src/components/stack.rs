//! Port of `packages/tui/src/components/stack.ts` @ pi 4181f66.
//!
//! `Stack` core: entry bookkeeping (`add_child`/`remove_child`/`clear`) plus
//! the size solver (`visible_stack_entries` / `clamp_size` / `distribute` /
//! `allocate_stack_sizes`). The non-layout fallback rendering lives in
//! `v_stack.rs` / `h_stack.rs`; `Stack::render` dispatches on `kind`.
//!
//! Intentional differences:
//! - Upstream `Stack extends Container` and keeps children twice (the
//!   `Container.children` array plus `entries`); here the entries list holds
//!   [`SharedComponent`]s and IS the child list.
//! - Upstream `Stack` is abstract with a `layoutType` class field; here `kind`
//!   is a constructor argument and `render` dispatches on it.
//! - Sizing numbers are `f64` throughout to mirror upstream's `Math.floor` /
//!   proportional arithmetic exactly; `Number.MAX_SAFE_INTEGER` becomes
//!   `f64::MAX`.
//! - `axis` is not modeled (upstream only allows vertical scrolling; stack
//!   direction is `kind` instead).

use crate::layout_node::{
    Basis, LayoutNode, LayoutViewport, StackAlign, StackEntry, StackKind, StackLayoutNode,
    StackVisibleFn,
};
use crate::tui::{lock_component, same_component, Component, SharedComponent};

/// `StackEntryOptions` (stack.ts:4-11): raw, unnormalized options accepted by
/// [`Stack::add_child`].
#[derive(Default)]
pub struct StackEntryOptions {
    pub basis: Option<Basis>,
    pub grow: Option<f64>,
    pub shrink: Option<f64>,
    pub min_size: Option<f64>,
    pub max_size: Option<f64>,
    pub visible: Option<StackVisibleFn>,
}

/// `StackChild` (stack.ts:17): a plain component or a component with options.
pub enum StackChild {
    Component(SharedComponent),
    Entry(SharedComponent, StackEntryOptions),
}

/// `StackOptions` (stack.ts:19-22).
#[derive(Default)]
pub struct StackOptions {
    pub gap: Option<f64>,
    pub align: Option<StackAlign>,
}

/// `normalizeSize` (stack.ts:28-30): undefined or non-finite → fallback, else
/// `max(0, floor(value))`.
fn normalize_size(value: Option<f64>, fallback: f64) -> f64 {
    match value {
        Some(value) if value.is_finite() => value.floor().max(0.0),
        _ => fallback,
    }
}

/// `Stack` (stack.ts:32-80). Children are shared components; identity
/// comparisons (`remove_child`) use `Arc::ptr_eq` like the rest of the TUI.
pub struct Stack {
    pub(crate) kind: StackKind,
    pub(crate) entries: Vec<StackEntry>,
    pub(crate) gap: f64,
    pub(crate) align: StackAlign,
}

impl Stack {
    /// Upstream constructor (stack.ts:38-46): normalizes `gap`, defaults
    /// `align` to stretch, then adds the initial children.
    pub(crate) fn new(kind: StackKind, children: Vec<StackChild>, options: StackOptions) -> Self {
        let mut stack = Stack {
            kind,
            entries: Vec::new(),
            gap: normalize_size(options.gap, 0.0),
            align: options.align.unwrap_or_default(),
        };
        for child in children {
            match child {
                StackChild::Component(component) => {
                    stack.add_child(component, StackEntryOptions::default())
                }
                StackChild::Entry(component, entry_options) => {
                    stack.add_child(component, entry_options)
                }
            }
        }
        stack
    }

    /// `addChild` (stack.ts:48-59): sizing options are normalized at insertion
    /// (`grow` default 0, `shrink` default 1, `minSize` default 0, `maxSize`
    /// default `MAX_SAFE_INTEGER`; all `max(0, floor)`, non-finite → default).
    /// `basis` and `visible` are stored as-is. Options left `None` stay absent
    /// so the solver applies the upstream `?? default` at use time.
    pub fn add_child(&mut self, component: SharedComponent, options: StackEntryOptions) {
        self.entries.push(StackEntry {
            component,
            basis: options.basis,
            grow: options.grow.map(|grow| normalize_size(Some(grow), 0.0)),
            shrink: options
                .shrink
                .map(|shrink| normalize_size(Some(shrink), 1.0)),
            min_size: options.min_size.map(|min| normalize_size(Some(min), 0.0)),
            max_size: options
                .max_size
                .map(|max| normalize_size(Some(max), f64::MAX)),
            visible: options.visible,
        });
    }

    /// `removeChild` (stack.ts:61-65): removes the first entry whose component
    /// is the same object.
    pub fn remove_child(&mut self, component: &SharedComponent) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| same_component(&entry.component, component))
        {
            self.entries.remove(index);
        }
    }

    /// `clear` (stack.ts:67-70).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// `[LAYOUT_NODE]()` (stack.ts:72-79).
    fn stack_layout_node(&self) -> LayoutNode<'_> {
        LayoutNode::Stack(StackLayoutNode {
            kind: self.kind,
            entries: &self.entries,
            gap: self.gap,
            align: self.align,
        })
    }
}

impl Component for Stack {
    fn render(&self, width: usize) -> Vec<String> {
        // Non-layout fallback: `VStack.render` / `HStack.render`
        // (v-stack.ts:10-30 / h-stack.ts:12-43).
        match self.kind {
            StackKind::Vertical => crate::components::v_stack::render_v_stack(self, width),
            StackKind::Horizontal => crate::components::h_stack::render_h_stack(self, width),
        }
    }

    fn invalidate(&mut self) {
        // `Container.invalidate` (tui.ts:229-233). Clone the child list first;
        // never hold this component's lock while locking children.
        let children: Vec<SharedComponent> = self
            .entries
            .iter()
            .map(|entry| entry.component.clone())
            .collect();
        for child in &children {
            lock_component(child).invalidate();
        }
    }

    fn shared_children(&self) -> Option<Vec<SharedComponent>> {
        Some(
            self.entries
                .iter()
                .map(|entry| entry.component.clone())
                .collect(),
        )
    }

    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        Some(self.stack_layout_node())
    }
}

/// `visibleStackEntries` (stack.ts:82-87). Clones the surviving entries so the
/// caller can release the stack's lock before rendering children.
pub fn visible_stack_entries(entries: &[StackEntry], viewport: LayoutViewport) -> Vec<StackEntry> {
    entries
        .iter()
        .filter(|entry| {
            entry
                .visible
                .as_ref()
                .is_none_or(|visible| visible(viewport))
        })
        .cloned()
        .collect()
}

/// `clampSize` (stack.ts:89-93).
fn clamp_size(size: f64, entry: &StackEntry) -> f64 {
    let min = entry.min_size.unwrap_or(0.0).floor().max(0.0);
    let max = entry.max_size.unwrap_or(f64::MAX).floor().max(min);
    size.floor().max(0.0).min(max).max(min)
}

/// `distribute` mode (stack.ts:99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DistributeMode {
    Grow,
    Shrink,
}

/// `distribute` (stack.ts:95-133): multi-round proportional distribution.
/// Grow weight is `grow`; shrink weight is `shrink * max(1, current size)`.
/// Each round proposes `max(1, floor(remaining * weight / totalWeight))`,
/// clamps to capacity, and recomputes weights until nothing moves.
fn distribute(sizes: &mut [f64], entries: &[StackEntry], amount: f64, mode: DistributeMode) {
    let mut remaining = amount;
    while remaining > 0.0 {
        let candidates: Vec<usize> = (0..entries.len())
            .filter(|&index| {
                let entry = &entries[index];
                match mode {
                    DistributeMode::Grow => {
                        entry.grow.unwrap_or(0.0) > 0.0
                            && sizes[index] < entry.max_size.unwrap_or(f64::MAX)
                    }
                    DistributeMode::Shrink => {
                        entry.shrink.unwrap_or(1.0) > 0.0
                            && sizes[index] > entry.min_size.unwrap_or(0.0)
                    }
                }
            })
            .collect();
        if candidates.is_empty() {
            return;
        }

        let weight = |sizes: &[f64], index: usize| -> f64 {
            let entry = &entries[index];
            match mode {
                DistributeMode::Grow => entry.grow.unwrap_or(0.0),
                DistributeMode::Shrink => entry.shrink.unwrap_or(1.0) * sizes[index].max(1.0),
            }
        };
        let total_weight: f64 = candidates.iter().map(|&index| weight(sizes, index)).sum();
        let mut distributed = 0.0;
        for &index in &candidates {
            if remaining <= 0.0 {
                break;
            }
            let proposed = ((remaining * weight(sizes, index)) / total_weight)
                .floor()
                .max(1.0);
            let entry = &entries[index];
            let capacity = match mode {
                DistributeMode::Grow => entry.max_size.unwrap_or(f64::MAX) - sizes[index],
                DistributeMode::Shrink => sizes[index] - entry.min_size.unwrap_or(0.0),
            };
            let delta = remaining.min(proposed).min(capacity);
            if delta <= 0.0 {
                continue;
            }
            sizes[index] += match mode {
                DistributeMode::Grow => delta,
                DistributeMode::Shrink => -delta,
            };
            remaining -= delta;
            distributed += delta;
        }
        if distributed == 0.0 {
            return;
        }
    }
}

/// `allocateStackSizes` (stack.ts:135-154): clamp intrinsic/basis sizes, then
/// grow or shrink toward the available size (minus gaps). With
/// `available_size == None` only clamping happens.
pub fn allocate_stack_sizes(
    entries: &[StackEntry],
    intrinsic_sizes: &[f64],
    available_size: Option<f64>,
    gap: f64,
) -> Vec<f64> {
    let mut sizes: Vec<f64> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let base = match entry.basis {
                None | Some(Basis::Auto) => intrinsic_sizes.get(index).copied().unwrap_or(0.0),
                Some(Basis::Fixed(basis)) => basis,
            };
            clamp_size(base, entry)
        })
        .collect();
    let Some(available_size) = available_size else {
        return sizes;
    };

    let content_size =
        (available_size.floor() - entries.len().saturating_sub(1) as f64 * gap).max(0.0);
    let total: f64 = sizes.iter().sum();
    if total < content_size {
        distribute(
            &mut sizes,
            entries,
            content_size - total,
            DistributeMode::Grow,
        );
    } else if total > content_size {
        distribute(
            &mut sizes,
            entries,
            total - content_size,
            DistributeMode::Shrink,
        );
    }
    sizes
}

#[cfg(test)]
mod tests {
    //! Solver matrix for `allocateStackSizes` / `distribute` / `clampSize`
    //! (stack.ts:89-154), plus the nested-minimum-size regression covered
    //! through `VStack::render` (f24ab6e14).

    use super::*;
    use crate::components::text::Text;
    use crate::components::v_stack::VStack;
    use crate::tui::shared_component;

    fn entry(options: StackEntryOptions) -> StackEntry {
        let mut stack = Stack::new(StackKind::Vertical, Vec::new(), StackOptions::default());
        stack.add_child(shared_component(Text::new("x", 0, 0, None)), options);
        stack.entries.pop().unwrap_or_else(|| unreachable!())
    }

    fn options() -> StackEntryOptions {
        StackEntryOptions::default()
    }

    fn allocate(entries: &[StackEntry], intrinsic: &[f64], available: Option<f64>) -> Vec<f64> {
        allocate_stack_sizes(entries, intrinsic, available, 0.0)
    }

    // ---- clamping / basis ----

    #[test]
    fn intrinsic_sizes_are_used_when_basis_is_unset_or_auto() {
        let entries = [
            entry(options()),
            entry(StackEntryOptions {
                basis: Some(Basis::Auto),
                ..options()
            }),
        ];
        assert_eq!(allocate(&entries, &[3.0, 5.0], None), vec![3.0, 5.0]);
    }

    #[test]
    fn fixed_basis_overrides_intrinsic_and_is_floored_by_clamp() {
        // `basis` is NOT normalized at add time; clampSize floors it.
        let entries = [entry(StackEntryOptions {
            basis: Some(Basis::Fixed(4.7)),
            ..options()
        })];
        assert_eq!(allocate(&entries, &[10.0], None), vec![4.0]);
    }

    #[test]
    fn min_and_max_size_clamp_intrinsic() {
        let entries = [
            entry(StackEntryOptions {
                min_size: Some(3.0),
                ..options()
            }),
            entry(StackEntryOptions {
                max_size: Some(4.0),
                ..options()
            }),
        ];
        assert_eq!(allocate(&entries, &[1.0, 10.0], None), vec![3.0, 4.0]);
    }

    #[test]
    fn normalize_size_floors_and_replaces_non_finite_with_defaults() {
        assert_eq!(normalize_size(Some(2.9), 0.0), 2.0);
        assert_eq!(normalize_size(Some(-3.0), 1.0), 0.0);
        assert_eq!(normalize_size(Some(f64::NAN), 1.0), 1.0);
        assert_eq!(normalize_size(Some(f64::INFINITY), 0.0), 0.0);
        assert_eq!(normalize_size(None, 7.0), 7.0);
    }

    // ---- grow ----

    #[test]
    fn grow_distributes_extra_space_in_floored_multi_round_shares() {
        let entries = [
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
        ];
        // 2 + 2 = 4 < 10, extra 6. Round 1: proposed 3 and 1 → 5 + 3;
        // round 2: proposed 1 each → 6 + 4. Earlier entries win the floor
        // remainder — deterministic, like upstream.
        assert_eq!(allocate(&entries, &[2.0, 2.0], Some(10.0)), vec![6.0, 4.0]);
    }

    #[test]
    fn grow_skips_entries_without_grow() {
        let entries = [
            entry(options()),
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
        ];
        assert_eq!(allocate(&entries, &[2.0, 2.0], Some(10.0)), vec![2.0, 8.0]);
    }

    #[test]
    fn grow_respects_max_size_and_redistributes() {
        let entries = [
            entry(StackEntryOptions {
                grow: Some(1.0),
                max_size: Some(4.0),
                ..options()
            }),
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
        ];
        // First entry caps at 4; the remainder goes to the second.
        assert_eq!(allocate(&entries, &[2.0, 2.0], Some(12.0)), vec![4.0, 8.0]);
    }

    #[test]
    fn grow_weights_only_approximate_the_ratio_after_flooring() {
        let entries = [
            entry(StackEntryOptions {
                grow: Some(2.0),
                ..options()
            }),
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
        ];
        // Extra 9 with weights 2:1. Round 1: floor(9*2/3)=6, floor(3*1/3)=1
        // → 6 + 1; round 2: floor(2*2/3)=1, max(1, floor(1/3))=1 → 7 + 2.
        assert_eq!(allocate(&entries, &[0.0, 0.0], Some(9.0)), vec![7.0, 2.0]);
    }

    // ---- shrink ----

    #[test]
    fn shrink_removes_overflow_proportionally_to_size() {
        let entries = [entry(options()), entry(options())];
        // Default shrink 1, weight = max(1, size): 8 and 2 → remove 5 as 4 + 1.
        assert_eq!(allocate(&entries, &[8.0, 2.0], Some(5.0)), vec![4.0, 1.0]);
    }

    #[test]
    fn shrink_respects_min_size_and_redistributes() {
        let entries = [
            entry(StackEntryOptions {
                min_size: Some(3.0),
                ..options()
            }),
            entry(options()),
        ];
        // Overflow 5: first entry floors at min 3, the rest comes off the second.
        assert_eq!(allocate(&entries, &[5.0, 5.0], Some(5.0)), vec![3.0, 2.0]);
    }

    #[test]
    fn shrink_zero_shrink_entries_are_untouched() {
        let entries = [
            entry(StackEntryOptions {
                shrink: Some(0.0),
                ..options()
            }),
            entry(options()),
        ];
        assert_eq!(allocate(&entries, &[4.0, 4.0], Some(6.0)), vec![4.0, 2.0]);
    }

    #[test]
    fn shrink_keeps_distributing_until_remaining_is_exhausted() {
        let entries = [entry(options()), entry(options())];
        // contentSize 2 < total 6, shrink 4. Round 1 (weights 3:3): 2 + 1 →
        // sizes 1 + 2; round 2 (weights 1:2): floor(1/3)→max(1,·)=1 comes off
        // the first entry → 0 + 2. Entries may shrink to 0, never below min.
        assert_eq!(allocate(&entries, &[3.0, 3.0], Some(2.0)), vec![0.0, 2.0]);
    }

    // ---- gap ----

    #[test]
    fn gap_is_subtracted_from_available_size() {
        let entries = [
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
            entry(StackEntryOptions {
                grow: Some(1.0),
                ..options()
            }),
        ];
        // available 11, gap 2 → content 9; 2 + 2 = 4 → +5 split 3 + 2.
        assert_eq!(
            allocate_stack_sizes(&entries, &[2.0, 2.0], Some(11.0), 2.0),
            vec![5.0, 4.0]
        );
    }

    #[test]
    fn single_entry_has_no_gap_cost() {
        let entries = [entry(StackEntryOptions {
            grow: Some(1.0),
            ..options()
        })];
        assert_eq!(
            allocate_stack_sizes(&entries, &[1.0], Some(6.0), 2.0),
            vec![6.0]
        );
    }

    // ---- combined basis + grow + shrink ----

    #[test]
    fn basis_then_grow_then_clamp_combined() {
        let entries = [
            entry(StackEntryOptions {
                basis: Some(Basis::Fixed(5.0)),
                grow: Some(1.0),
                max_size: Some(6.0),
                ..options()
            }),
            entry(StackEntryOptions {
                grow: Some(2.0),
                min_size: Some(1.0),
                ..options()
            }),
        ];
        // content 10; bases 5 + 2 = 7, extra 3: weights 1:2 → round 1 gives
        // floor(3*1/3)=1 (caps at 6) and floor(3*2/3)=2 → sizes 6 + 4.
        assert_eq!(allocate(&entries, &[0.0, 2.0], Some(10.0)), vec![6.0, 4.0]);
    }

    // ---- nested minimum sizes regression (f24ab6e14) ----

    /// Mock whose render output height depends on the width it is given: a
    /// two-line text wrapped by `Text` inside an inner `VStack` with a fixed
    /// basis entry on top. Before f24ab6e14 the outer fallback measured only
    /// the inner stack's own render height and dropped the fixed entry.
    #[test]
    fn nested_stack_minimum_sizes_are_included_in_fallback_render() {
        let inner = VStack::new(
            vec![
                StackChild::Entry(
                    shared_component(Text::new("header", 0, 0, None)),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(2.0)),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Component(shared_component(Text::new("body", 0, 0, None))),
            ],
            StackOptions::default(),
        );
        let mut outer = VStack::new(Vec::new(), StackOptions::default());
        outer.add_child(
            shared_component(inner),
            StackEntryOptions {
                min_size: Some(4.0),
                ..StackEntryOptions::default()
            },
        );
        let lines = outer.render(10);
        // Inner fallback: header clamps to basis 2 (1 line rendered + 1 pad),
        // body renders 1 line → 3 lines total; the outer min_size 4 pads one
        // more empty line. Stack padding lines are "" (v-stack.ts:23-27),
        // while `Text` pads its own output to the full width.
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "header    ");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "body      ");
        assert_eq!(lines[3], "");
    }

    #[test]
    fn remove_child_removes_first_matching_entry() {
        let a = shared_component(Text::new("a", 0, 0, None));
        let b = shared_component(Text::new("b", 0, 0, None));
        let mut stack = Stack::new(StackKind::Vertical, Vec::new(), StackOptions::default());
        stack.add_child(a.clone(), StackEntryOptions::default());
        stack.add_child(b.clone(), StackEntryOptions::default());
        stack.add_child(a.clone(), StackEntryOptions::default());
        stack.remove_child(&a);
        assert_eq!(stack.entries.len(), 2);
        assert!(same_component(&stack.entries[0].component, &b));
        assert!(same_component(&stack.entries[1].component, &a));
        stack.clear();
        assert!(stack.entries.is_empty());
    }

    #[test]
    fn visible_stack_entries_filters_by_predicate() {
        let hidden = entry(StackEntryOptions {
            visible: Some(std::sync::Arc::new(|_| false)),
            ..options()
        });
        let shown = entry(options());
        let viewport = LayoutViewport {
            width: 10,
            height: 10,
        };
        let visible = visible_stack_entries(&[hidden, shown.clone()], viewport);
        assert_eq!(visible.len(), 1);
        assert!(same_component(&visible[0].component, &shown.component));
    }
}
