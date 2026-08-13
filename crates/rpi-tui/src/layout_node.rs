//! Port of `packages/tui/src/layout-node.ts` @ pi 4181f66.
//!
//! Layout node types: the data components hand to the layout engine so it can
//! measure and place stack / scroll containers without going through
//! `Component::render`.
//!
//! Intentional differences:
//! - The `LAYOUT_NODE` symbol protocol (`getLayoutNode`, layout-node.ts:48-51)
//!   is replaced by the defaulted `Component::layout_node` trait method (see
//!   the `tui.rs` header note on the T30 contract extension).
//! - `LayoutViewport` uses `usize`; the unbounded viewport height upstream
//!   expresses as `Number.MAX_SAFE_INTEGER` becomes `usize::MAX`. It only
//!   feeds `visible` predicates, never solver arithmetic.
//! - `StackLayoutEntry.visible` is an `Arc<dyn Fn>` so entries stay `Clone`
//!   (the engine clones entry lists out before releasing component locks).
//! - Entry sizing options are `f64` and stored normalized (see
//!   `components/stack.rs`); `Number.MAX_SAFE_INTEGER` becomes `f64::MAX`.

use std::sync::Arc;

use crate::components::scroll_view::Overscroll;
use crate::tui::{RenderHandle, SharedComponent};

/// `LayoutViewport` (layout-node.ts:5-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutViewport {
    pub width: usize,
    pub height: usize,
}

/// `StackLayoutNode.type` (layout-node.ts:21): `"vstack" | "hstack"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackKind {
    Vertical,
    Horizontal,
}

/// `StackLayoutNode.align` (layout-node.ts:24): `"stretch" | "start" | "center" | "end"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackAlign {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// `StackLayoutEntry.basis` (layout-node.ts:12): `number | "auto"`; `None` on
/// the entry mirrors upstream `basis === undefined`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Basis {
    Auto,
    /// NOT normalized on input (upstream never floors `basis`; `clampSize`
    /// does it at allocation time), so fractional values are preserved.
    Fixed(f64),
}

/// `visible?: (viewport: LayoutViewport) => boolean` (layout-node.ts:17).
pub type StackVisibleFn = Arc<dyn Fn(LayoutViewport) -> bool + Send + Sync>;

/// `StackLayoutEntry` (layout-node.ts:10-18). Sizing options are stored
/// normalized (`max(0, floor)`, non-finite → default) by `Stack::add_child`,
/// exactly like upstream's `addChild`; `basis` is stored raw.
#[derive(Clone)]
pub struct StackEntry {
    pub component: SharedComponent,
    pub basis: Option<Basis>,
    pub grow: Option<f64>,
    pub shrink: Option<f64>,
    pub min_size: Option<f64>,
    pub max_size: Option<f64>,
    pub visible: Option<StackVisibleFn>,
}

/// `StackLayoutNode` (layout-node.ts:20-25). Borrows the stack's live entry
/// list like upstream's array reference; the engine clones what it needs and
/// releases the component lock before recursing.
pub struct StackLayoutNode<'a> {
    pub kind: StackKind,
    pub entries: &'a [StackEntry],
    pub gap: f64,
    pub align: StackAlign,
}

/// `ScrollLayoutState` (layout-node.ts:27-34). All methods take `&self`;
/// implementations use interior mutability (see `components/scroll_view.rs`).
pub trait ScrollLayoutState {
    /// Upstream `readonly scrollTop`.
    fn scroll_top(&self) -> usize;
    /// Upstream `readonly primary`.
    fn primary(&self) -> bool;
    /// Upstream `readonly overscroll`.
    fn overscroll(&self) -> Overscroll;
    /// Upstream `readonly viewportHeight`.
    fn viewport_height(&self) -> usize;
    /// Upstream `getContentWidth(width)`.
    fn content_width(&self, width: usize) -> usize;
    /// Upstream `updateLayout(contentHeight, viewportHeight, requestRender)`.
    fn update_layout(
        &self,
        content_height: usize,
        viewport_height: usize,
        request_render: RenderHandle,
    );
}

/// `ScrollLayoutNode` (layout-node.ts:36-40). `component` is a cloned shared
/// reference (upstream holds the live child object); `state` borrows the
/// scroll view.
pub struct ScrollLayoutNode<'a> {
    pub component: SharedComponent,
    pub state: &'a dyn ScrollLayoutState,
}

/// `LayoutNode` (layout-node.ts:42).
pub enum LayoutNode<'a> {
    Stack(StackLayoutNode<'a>),
    Scroll(ScrollLayoutNode<'a>),
}
