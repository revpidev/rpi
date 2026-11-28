//! Port of `packages/tui/src/layout.ts` @ pi 9841914.
//!
//! The layout engine: walks the component tree via `Component::layout_node`
//! (the T30 replacement for upstream's `LAYOUT_NODE` symbol protocol),
//! measures stack/scroll containers, and paints the frame into a line buffer.
//! The redesigned full-track scrollbar overlay (457ae8c79) ships here.
//!
//! Intentional differences:
//! - `LayoutBox.parent` is omitted — upstream never reads it (it is only
//!   written by `withParent` / `childBox.parent = box`).
//! - `LayoutRect` coordinates are `isize` (scroll translation can make
//!   `rect.y` negative); `width`/`height` are `usize` and stay non-negative.
//!   Upstream `Math.round` inputs are all non-negative, so `f64::round` is
//!   equivalent. Solver arithmetic stays `f64` (see `components/stack.rs`).
//! - Components are [`SharedComponent`]s; identity comparisons (`===`
//!   upstream) use `Arc::ptr_eq`. The render cache is keyed by the `Arc`
//!   pointer address instead of JS object identity, and is rebuilt per frame
//!   (`renderLayoutFrame` creates a fresh context, layout.ts:361-366).
//! - V19-06 (ADR-0049 decision 4): [`render_layout_frame_cached`] adds a
//!   renderer-owned cross-frame [`LayoutRenderCache`] behind the frame-local
//!   one — a trusted component (instance id + revision) whose width did not
//!   change is served without a `render()` call, a hit being one `Arc` clone.
//!   A scroll child that is a transcript document is measured through its
//!   height index: the child box carries only the viewport window (plus
//!   overscan, see `LayoutBox::line_window_start`) and `scroll_content_lines`
//!   stays `None`; the full-text consumers go through
//!   [`LayoutBox::scroll_content`]. Frames stay byte-identical to the
//!   full-render path (see `tests/transcript.rs`).
//! - `LayoutBox.scroll_view` / `LayoutFrame.primary_scroll_view` hold the
//!   SharedComponent of the scroll view; `ScrollView` access goes through
//!   `Component::as_scroll_view` under the component lock. Lock discipline:
//!   a component's lock is only held to read/clone values out of it and is
//!   released before recursing into children.
//! - `layer` is kept but always 0 (upstream never sets anything else here).
//! - `LayoutContext.requestRender` is a [`RenderHandle`] (T28 precedent)
//!   instead of a plain closure.
//! - JS sparse-array semantics are made explicit: painting the cropped
//!   scroll-top residual image checks `rect.y` against the screen bounds
//!   instead of relying on auto-extension (layout.ts:342).
//! - The OSC 133 zone-prefix strip (`OSC133_ZONE_PREFIX`, layout.ts:8) is a
//!   hand-rolled loop, not a regex.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::components::scroll_view::ScrollbarMode;
use crate::components::stack::{allocate_stack_sizes, visible_stack_entries};
use crate::layout_cache::LayoutRenderCache;
use crate::layout_node::{Basis, LayoutNode, LayoutViewport, StackAlign, StackKind};
use crate::terminal_image::{crop_kitty_image_line, get_kitty_image_metadata, is_image_line};
use crate::transcript::{TranscriptDocument, clamp_window};
use crate::tui::{
    CURSOR_MARKER, RenderHandle, SharedComponent, composite_tui_line, lock_component,
};
use crate::utils::{
    extract_ansi_code, get_active_background_ansi, get_grapheme_cell_range, slice_by_column,
    visible_width,
};

/// `LayoutRect` (layout.ts:10-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: isize,
    pub y: isize,
    pub width: usize,
    pub height: usize,
}

/// `LayoutBox` (layout.ts:17-28), minus `parent` (see header note).
pub struct LayoutBox {
    pub component: SharedComponent,
    pub rect: LayoutRect,
    pub clip: LayoutRect,
    pub children: Vec<LayoutBox>,
    pub lines: Option<Arc<[String]>>,
    pub line_offset: usize,
    pub scroll_view: Option<SharedComponent>,
    /// Full content lines of the scroll child (non-transcript documents, and
    /// the implicit document). `None` for a transcript document measured
    /// through its height index (V19-06): consumers use
    /// [`LayoutBox::scroll_content`] instead of reading this field.
    pub scroll_content_lines: Option<Arc<[String]>>,
    /// The scroll child is a transcript document whose content is
    /// materialized on demand (V19-06 virtual mode).
    pub scroll_content_virtual: bool,
    /// Content row of `lines[0]` (0 for a complete line set; the viewport
    /// window's first row for a virtualized transcript document, V19-06).
    pub line_window_start: usize,
    /// Kept for parity; always 0 (layout.ts:27).
    pub layer: i32,
}

/// Rows of a scroll view's content for the full-text consumers (scrollbar,
/// prompt jump, search, selection, Kitty residual crops). `Full` is the
/// classic whole-content array; `Virtual` answers from the transcript
/// document's height index and retained lines (V19-06).
pub enum ScrollContent {
    Full(Arc<[String]>),
    Virtual {
        document: SharedComponent,
        width: usize,
        height: usize,
    },
}

impl ScrollContent {
    /// Content height in rows.
    pub fn len(&self) -> usize {
        match self {
            ScrollContent::Full(lines) => lines.len(),
            ScrollContent::Virtual { height, .. } => *height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One content row, or `None` past the end.
    pub fn line(&self, row: usize) -> Option<String> {
        match self {
            ScrollContent::Full(lines) => lines.get(row).cloned(),
            ScrollContent::Virtual {
                document,
                width,
                height,
            } => {
                if row >= *height {
                    return None;
                }
                with_transcript(document, |document| document.line(row, *width)).flatten()
            }
        }
    }

    /// The content rows `rows` (clamped to the content).
    pub fn rows(&self, rows: Range<usize>) -> Arc<[String]> {
        match self {
            ScrollContent::Full(lines) => {
                let end = rows.end.min(lines.len());
                let start = rows.start.min(end);
                lines[start..end].to_vec().into()
            }
            ScrollContent::Virtual {
                document,
                width,
                height,
            } => {
                let end = rows.end.min(*height);
                let start = rows.start.min(end);
                with_transcript(document, |document| document.rows(start..end, *width))
                    .unwrap_or_else(|| Arc::from(Vec::new()))
            }
        }
    }

    /// The whole content. `Virtual` materializes the lazy full-text corpus
    /// (search); it is cached per content stamp and released when a frame
    /// goes by without a consumer.
    pub fn to_lines(&self) -> Arc<[String]> {
        match self {
            ScrollContent::Full(lines) => lines.clone(),
            ScrollContent::Virtual {
                document, width, ..
            } => with_transcript(document, |document| document.materialize(*width))
                .unwrap_or_else(|| Arc::from(Vec::new())),
        }
    }
}

/// Run `f` with the transcript document inside `component`, if it is one.
pub fn with_transcript<R>(
    component: &SharedComponent,
    f: impl FnOnce(&dyn TranscriptDocument) -> R,
) -> Option<R> {
    let guard = lock_component(component);
    let document = guard.as_transcript_document()?;
    Some(f(document))
}

/// Rows the virtual scroll path materializes beyond the viewport on each
/// side, so small scrolls and the Kitty residual crop find their rows
/// without an on-demand render (V19-06 FR-B R2, "a little overscan").
pub const TRANSCRIPT_OVERSCAN_ROWS: usize = 16;

impl LayoutBox {
    /// The content rows of this scroll box for the full-text consumers
    /// (V19-06). `None` when the box is not a scroll view.
    pub fn scroll_content(&self) -> Option<ScrollContent> {
        self.scroll_view.as_ref()?;
        if let Some(lines) = &self.scroll_content_lines {
            return Some(ScrollContent::Full(lines.clone()));
        }
        if !self.scroll_content_virtual {
            return None;
        }
        let child = self.children.first()?;
        Some(ScrollContent::Virtual {
            document: child.component.clone(),
            width: child.rect.width,
            height: child.rect.height,
        })
    }
}

/// `LayoutFrame` (layout.ts:30-36).
pub struct LayoutFrame {
    pub root: LayoutBox,
    pub width: usize,
    pub height: usize,
    pub lines: Vec<String>,
    pub primary_scroll_view: Option<SharedComponent>,
}

/// `ScrollbarGeometry` (layout.ts:38-45).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: isize,
    pub track_top: isize,
    pub track_height: usize,
    pub thumb_top: isize,
    pub thumb_height: usize,
    pub max_scroll_top: usize,
}

/// `LayoutContext` (layout.ts:47-52). Created fresh by every
/// [`render_layout_frame`] call.
struct LayoutContext<'a> {
    viewport: LayoutViewport,
    /// Keyed by `Arc::as_ptr` address, then by width (see header note).
    render_cache: HashMap<usize, HashMap<usize, Arc<[String]>>>,
    /// Cross-frame cache of trusted components (V19-06 FR-A), owned by the
    /// renderer.
    cache: &'a mut LayoutRenderCache,
    request_render: RenderHandle,
    primary_scroll_view: Option<SharedComponent>,
}

/// `intersect` (layout.ts:54-60).
fn intersect(a: &LayoutRect, b: &LayoutRect) -> LayoutRect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width as isize).min(b.x + b.width as isize);
    let bottom = (a.y + a.height as isize).min(b.y + b.height as isize);
    LayoutRect {
        x,
        y,
        width: (right - x).max(0) as usize,
        height: (bottom - y).max(0) as usize,
    }
}

/// `renderCached` (layout.ts:62-75). Widths are already `usize`; the
/// `max(1, floor(width))` normalization still applies to zero widths.
///
/// V19-06: the frame-local cache still dedups renders inside the frame; for
/// a trusted component the lines come from the cross-frame
/// [`LayoutRenderCache`] (hit = one `Arc` clone, zero `render()` calls), and
/// only a miss calls `render`. Untrusted components render every frame and
/// are never stored.
fn render_cached(
    context: &mut LayoutContext,
    component: &SharedComponent,
    width: usize,
) -> Arc<[String]> {
    let safe_width = width.max(1);
    let key = Arc::as_ptr(component) as usize;
    if let Some(lines) = context
        .render_cache
        .get(&key)
        .and_then(|widths| widths.get(&safe_width))
    {
        return lines.clone();
    }
    let lines = {
        let guard = lock_component(component);
        match guard.cache_revision() {
            Some(revision) => match context.cache.get(component, safe_width, revision) {
                Some(lines) => lines,
                None => {
                    let lines: Arc<[String]> = guard.render(safe_width).into();
                    context
                        .cache
                        .insert(component.clone(), safe_width, revision, lines.clone());
                    lines
                }
            },
            None => {
                context.cache.note_untrusted_render();
                guard.render(safe_width).into()
            }
        }
    };
    context
        .render_cache
        .entry(key)
        .or_default()
        .insert(safe_width, lines.clone());
    lines
}

/// `measureHeight` (layout.ts:77-79).
fn measure_height(context: &mut LayoutContext, component: &SharedComponent, width: usize) -> usize {
    render_cached(context, component, width).len()
}

/// `measureWidth` (layout.ts:81-83).
fn measure_width(context: &mut LayoutContext, component: &SharedComponent, width: usize) -> usize {
    render_cached(context, component, width)
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or(0)
}

/// `translateBox` (layout.ts:90-93).
fn translate_box(layout_box: &mut LayoutBox, delta_y: isize) {
    layout_box.rect.y += delta_y;
    for child in &mut layout_box.children {
        translate_box(child, delta_y);
    }
}

/// `updateClips` (layout.ts:95-98).
fn update_clips(layout_box: &mut LayoutBox, parent_clip: &LayoutRect) {
    layout_box.clip = intersect(parent_clip, &layout_box.rect);
    let clip = layout_box.clip;
    for child in &mut layout_box.children {
        update_clips(child, &clip);
    }
}

/// What `Component::layout_node` told us, cloned out so the component lock
/// can be released before recursing (layout-node.ts).
enum ExtractedNode {
    Leaf,
    Stack {
        kind: StackKind,
        entries: Vec<crate::layout_node::StackEntry>,
        gap: f64,
        align: StackAlign,
    },
    Scroll {
        child: SharedComponent,
    },
}

fn extract_node(component: &SharedComponent) -> ExtractedNode {
    let guard = lock_component(component);
    match guard.layout_node() {
        None => ExtractedNode::Leaf,
        Some(LayoutNode::Stack(node)) => ExtractedNode::Stack {
            kind: node.kind,
            entries: node.entries.to_vec(),
            gap: node.gap,
            align: node.align,
        },
        Some(LayoutNode::Scroll(node)) => ExtractedNode::Scroll {
            child: node.component.clone(),
        },
    }
}

/// `layoutComponent` (layout.ts:100-241).
fn layout_component(
    context: &mut LayoutContext,
    component: &SharedComponent,
    x: isize,
    y: isize,
    width: usize,
    height: Option<usize>,
    clip: &LayoutRect,
) -> LayoutBox {
    let safe_width = width.max(1);
    match extract_node(component) {
        ExtractedNode::Leaf => {
            // Leaf branch (layout.ts:111-128).
            let lines = render_cached(context, component, safe_width);
            let allocated_height = height.unwrap_or(lines.len());
            let mut line_offset = 0usize;
            if lines.len() > allocated_height && allocated_height > 0 {
                // Scroll the hardware-cursor line (CURSOR_MARKER) into view.
                if let Some(cursor_line) =
                    lines.iter().position(|line| line.contains(CURSOR_MARKER))
                    && cursor_line >= allocated_height
                {
                    line_offset = cursor_line - allocated_height + 1;
                }
            }
            let rect = LayoutRect {
                x,
                y,
                width: safe_width,
                height: allocated_height,
            };
            LayoutBox {
                component: component.clone(),
                rect,
                clip: intersect(clip, &rect),
                children: Vec::new(),
                lines: Some(lines),
                line_offset,
                scroll_view: None,
                scroll_content_lines: None,
                scroll_content_virtual: false,
                line_window_start: 0,
                layer: 0,
            }
        }
        ExtractedNode::Scroll { child } => {
            // Scroll branch (layout.ts:130-162). The child is laid out at the
            // pre-update scroll offset, `updateLayout` settles the real
            // scroll position, and the subtree is translated by the
            // difference. State access goes through the node's
            // `ScrollLayoutState`; the lock is released before recursing.
            let (previous_scroll_top, content_width) = {
                let guard = lock_component(component);
                match guard.layout_node() {
                    Some(LayoutNode::Scroll(node)) => (
                        node.state.scroll_top(),
                        node.state.content_width(safe_width),
                    ),
                    // Node kind cannot change between extract_node and here
                    // for a fixed component; fall back to leaf-like values.
                    _ => (0, safe_width),
                }
            };
            let is_transcript = lock_component(&child).as_transcript_document().is_some();
            if is_transcript {
                return layout_transcript_scroll(
                    context,
                    component,
                    &child,
                    x,
                    y,
                    safe_width,
                    height,
                    clip,
                    previous_scroll_top,
                    content_width,
                );
            }
            let child_box = layout_component(
                context,
                &child,
                x,
                y - previous_scroll_top as isize,
                content_width,
                None,
                clip,
            );
            let content_height = child_box.rect.height;
            let viewport_height = height.unwrap_or(content_height);
            let new_scroll_top = {
                let guard = lock_component(component);
                match guard.layout_node() {
                    Some(LayoutNode::Scroll(node)) => {
                        node.state.update_layout(
                            content_height,
                            viewport_height,
                            context.request_render.clone(),
                        );
                        // `node.state.primary || !context.primaryScrollView`
                        // (layout.ts:147): the first scroll view is implicitly
                        // primary; a later `primary: true` one overrides it.
                        if node.state.primary() || context.primary_scroll_view.is_none() {
                            context.primary_scroll_view = Some(component.clone());
                        }
                        node.state.scroll_top()
                    }
                    _ => previous_scroll_top,
                }
            };
            let mut child_box = child_box;
            translate_box(
                &mut child_box,
                previous_scroll_top as isize - new_scroll_top as isize,
            );
            let rect = LayoutRect {
                x,
                y,
                width: safe_width,
                height: viewport_height,
            };
            let child_clip = intersect(clip, &rect);
            let scroll_content_lines = render_cached(context, &child, content_width);
            let mut layout_box = LayoutBox {
                component: component.clone(),
                rect,
                clip: child_clip,
                children: vec![child_box],
                lines: None,
                line_offset: 0,
                scroll_view: Some(component.clone()),
                scroll_content_lines: Some(scroll_content_lines),
                scroll_content_virtual: false,
                line_window_start: 0,
                layer: 0,
            };
            update_clips(&mut layout_box.children[0], &child_clip);
            layout_box
        }
        ExtractedNode::Stack {
            kind,
            entries,
            gap,
            align,
        } => {
            let entries = visible_stack_entries(&entries, context.viewport);
            let gap_total = entries.len().saturating_sub(1) as f64 * gap;
            match kind {
                StackKind::Vertical => {
                    // VStack branch (layout.ts:166-192): numeric-basis entries
                    // are NOT rendered for measurement — the basis is the
                    // intrinsic height.
                    let intrinsic_heights: Vec<f64> = entries
                        .iter()
                        .map(|entry| match entry.basis {
                            Some(Basis::Fixed(basis)) => basis,
                            _ => measure_height(context, &entry.component, safe_width) as f64,
                        })
                        .collect();
                    let sizes = allocate_stack_sizes(
                        &entries,
                        &intrinsic_heights,
                        height.map(|h| h as f64),
                        gap,
                    );
                    let natural_height = sizes.iter().sum::<f64>() + gap_total;
                    let allocated_height = height.unwrap_or(natural_height as usize);
                    let rect = LayoutRect {
                        x,
                        y,
                        width: safe_width,
                        height: allocated_height,
                    };
                    let box_clip = intersect(clip, &rect);
                    let mut children = Vec::with_capacity(entries.len());
                    let mut child_y = y;
                    for (index, entry) in entries.iter().enumerate() {
                        // Sizes come out of `clamp_size` floored.
                        let size = sizes[index] as usize;
                        children.push(layout_component(
                            context,
                            &entry.component,
                            x,
                            child_y,
                            safe_width,
                            Some(size),
                            &box_clip,
                        ));
                        child_y += (sizes[index] + gap) as isize;
                    }
                    LayoutBox {
                        component: component.clone(),
                        rect,
                        clip: box_clip,
                        children,
                        lines: None,
                        line_offset: 0,
                        scroll_view: None,
                        scroll_content_lines: None,
                        scroll_content_virtual: false,
                        line_window_start: 0,
                        layer: 0,
                    }
                }
                StackKind::Horizontal => {
                    // HStack branch (layout.ts:194-240).
                    let intrinsic_widths: Vec<f64> = entries
                        .iter()
                        .map(|entry| match entry.basis {
                            Some(Basis::Fixed(basis)) => basis,
                            _ => measure_width(context, &entry.component, safe_width) as f64,
                        })
                        .collect();
                    let widths = allocate_stack_sizes(
                        &entries,
                        &intrinsic_widths,
                        Some(safe_width as f64),
                        gap,
                    );
                    let intrinsic_heights: Vec<usize> = entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| {
                            measure_height(
                                context,
                                &entry.component,
                                (widths[index] as usize).max(1),
                            )
                        })
                        .collect();
                    let allocated_height = height
                        .unwrap_or_else(|| intrinsic_heights.iter().copied().max().unwrap_or(0));
                    let rect = LayoutRect {
                        x,
                        y,
                        width: safe_width,
                        height: allocated_height,
                    };
                    let box_clip = intersect(clip, &rect);
                    let mut children = Vec::with_capacity(entries.len());
                    let mut child_x = x;
                    for (index, entry) in entries.iter().enumerate() {
                        let natural_child_height = intrinsic_heights[index];
                        let child_height = if align == StackAlign::Stretch {
                            allocated_height
                        } else {
                            allocated_height.min(natural_child_height)
                        };
                        let mut child_y = y;
                        if align == StackAlign::Center {
                            child_y += (allocated_height - child_height) as isize / 2;
                        } else if align == StackAlign::End {
                            child_y += (allocated_height - child_height) as isize;
                        }
                        let child_width = widths[index] as usize;
                        if child_width == 0 {
                            // Zero-width placeholder box (layout.ts:221-229):
                            // never laid out, never painted.
                            let zero_rect = LayoutRect {
                                x: child_x,
                                y: child_y,
                                width: 0,
                                height: child_height,
                            };
                            children.push(LayoutBox {
                                component: entry.component.clone(),
                                rect: zero_rect,
                                clip: LayoutRect {
                                    x: child_x,
                                    y: child_y,
                                    width: 0,
                                    height: 0,
                                },
                                children: Vec::new(),
                                lines: None,
                                line_offset: 0,
                                scroll_view: None,
                                scroll_content_lines: None,
                                scroll_content_virtual: false,
                                line_window_start: 0,
                                layer: 0,
                            });
                        } else {
                            children.push(layout_component(
                                context,
                                &entry.component,
                                child_x,
                                child_y,
                                child_width,
                                Some(child_height),
                                &box_clip,
                            ));
                        }
                        child_x += (widths[index] + gap) as isize;
                    }
                    LayoutBox {
                        component: component.clone(),
                        rect,
                        clip: box_clip,
                        children,
                        lines: None,
                        line_offset: 0,
                        scroll_view: None,
                        scroll_content_lines: None,
                        scroll_content_virtual: false,
                        line_window_start: 0,
                        layer: 0,
                    }
                }
            }
        }
    }
}

/// Scroll branch for a transcript document (V19-06 FR-B): the content height
/// comes from the document's height index instead of a full render, and the
/// child box carries only the rows of the viewport window (plus overscan),
/// starting at `line_offset`. Everything downstream (scrollbar geometry,
/// clip-limited painting, `updateLayout`, follow-end) sees the same heights
/// and rows the full render would produce; the full-text consumers read other
/// rows through [`LayoutBox::scroll_content`].
#[allow(clippy::too_many_arguments)]
fn layout_transcript_scroll(
    context: &mut LayoutContext,
    component: &SharedComponent,
    child: &SharedComponent,
    x: isize,
    y: isize,
    width: usize,
    height: Option<usize>,
    clip: &LayoutRect,
    previous_scroll_top: usize,
    content_width: usize,
) -> LayoutBox {
    let content_height = with_transcript(child, |document| document.sync(content_width))
        .map_or(0, |sync| sync.height);
    let viewport_height = height.unwrap_or(content_height);
    let new_scroll_top = {
        let guard = lock_component(component);
        match guard.layout_node() {
            Some(LayoutNode::Scroll(node)) => {
                node.state.update_layout(
                    content_height,
                    viewport_height,
                    context.request_render.clone(),
                );
                if node.state.primary() || context.primary_scroll_view.is_none() {
                    context.primary_scroll_view = Some(component.clone());
                }
                node.state.scroll_top()
            }
            _ => previous_scroll_top,
        }
    };
    let window = clamp_window(
        new_scroll_top,
        viewport_height,
        TRANSCRIPT_OVERSCAN_ROWS,
        content_height,
    );
    let window_lines = with_transcript(child, |document| {
        document.rows(window.clone(), content_width)
    })
    .unwrap_or_else(|| Arc::from(Vec::new()));
    let child_box = LayoutBox {
        component: child.clone(),
        rect: LayoutRect {
            x,
            y: y - previous_scroll_top as isize,
            width: content_width,
            height: content_height,
        },
        clip: *clip,
        children: Vec::new(),
        lines: Some(window_lines),
        line_offset: 0,
        scroll_view: None,
        scroll_content_lines: None,
        scroll_content_virtual: false,
        // The window's first row is the first painted line.
        line_window_start: window.start,
        layer: 0,
    };
    let rect = LayoutRect {
        x,
        y,
        width,
        height: viewport_height,
    };
    let child_clip = intersect(clip, &rect);
    let mut layout_box = LayoutBox {
        component: component.clone(),
        rect,
        clip: child_clip,
        children: vec![child_box],
        lines: None,
        line_offset: 0,
        scroll_view: Some(component.clone()),
        scroll_content_lines: None,
        scroll_content_virtual: true,
        line_window_start: 0,
        layer: 0,
    };
    // Same order as the classic branch: settle the offset first, then clip
    // the content box against its final position.
    translate_box(
        &mut layout_box.children[0],
        previous_scroll_top as isize - new_scroll_top as isize,
    );
    update_clips(&mut layout_box.children[0], &child_clip);
    layout_box
}

/// `replaceScrollbarCell` (layout.ts:249-278 @ 9841914, 457ae8c79):
/// overlay the scrollbar glyph onto the cell at `column`, REPLACING the
/// underlying grapheme. Wide graphemes spanning the column are replaced
/// whole (padded); `preserve_target_background` re-emits the target cell's
/// active background color after the reset/hyperlink-close prefix so the
/// overlay keeps the underlying background instead of inheriting the
/// replaced cell's foreground styling.
fn replace_scrollbar_cell(
    line: &str,
    column: usize,
    total_width: usize,
    replacement: &str,
    preserve_target_background: bool,
) -> String {
    if is_image_line(line) {
        return line.to_string();
    }

    let grapheme_range = get_grapheme_cell_range(line, column);
    let start = grapheme_range.map_or(column, |range| range.start);
    let end = grapheme_range.map_or(column + 1, |range| range.end);
    let before = slice_by_column(line, 0, start, true);
    let target = slice_by_column(line, start, end - start, true);
    let after = slice_by_column(line, end, total_width.saturating_sub(end), true);

    // The target's leading ANSI run (kept as the background source only;
    // the glyph itself is replaced, so the codes are NOT re-emitted).
    let mut target_prefix = String::new();
    let mut target_index = 0;
    while target_index < target.len() {
        let Some(ansi) = extract_ansi_code(&target, target_index) else {
            break;
        };
        target_prefix.push_str(ansi.code);
        target_index += ansi.length;
    }
    let before_padding = " ".repeat(start.saturating_sub(visible_width(&before)));
    let cell_padding_before = " ".repeat(column.saturating_sub(start));
    let cell_padding_after = " ".repeat(end.saturating_sub(column + 1));
    let background = if preserve_target_background {
        get_active_background_ansi(&target_prefix)
    } else {
        String::new()
    };
    // `\x1b[0m\x1b]8;;\x07` (layout.ts:276): reset styles, close any open
    // OSC 8 hyperlink, then optionally re-open the preserved background.
    format!(
        "{before}{before_padding}\x1b[0m\x1b]8;;\x07{background}{cell_padding_before}{replacement}{cell_padding_after}{after}"
    )
}

/// `getScrollbarGeometry` (layout.ts:280-307 @ 9841914, 457ae8c79).
/// `include_hidden_auto` reveals a hidden `auto` scrollbar with overflow
/// (the hover hit-test uses it to wake the track as the pointer enters).
pub fn get_scrollbar_geometry(
    layout_box: &LayoutBox,
    include_hidden_auto: bool,
) -> Option<ScrollbarGeometry> {
    let shared = layout_box.scroll_view.as_ref()?;
    if layout_box.rect.width == 0 || layout_box.rect.height == 0 {
        return None;
    }
    let (is_visible, scroll_top, scrollbar) = {
        let guard = lock_component(shared);
        let scroll_view = guard.as_scroll_view()?;
        (
            scroll_view.is_scrollbar_visible(),
            scroll_view.scroll_top(),
            scroll_view.scrollbar(),
        )
    };

    let content_height = layout_box
        .children
        .first()
        .map(|child| child.rect.height)
        .or_else(|| {
            layout_box
                .scroll_content_lines
                .as_ref()
                .map(|lines| lines.len())
        })
        .unwrap_or(0);
    let track_height = layout_box.rect.height;
    let can_reveal_hidden_auto =
        include_hidden_auto && scrollbar == ScrollbarMode::Auto && content_height > track_height;
    if !is_visible && !can_reveal_hidden_auto {
        return None;
    }

    // f64 so content_height == 0 yields +inf and `min(track, inf) == track`,
    // exactly like upstream's division by zero.
    let track = track_height as f64;
    let content = content_height as f64;
    // `minThumbHeight = min(2, trackHeight)`; a 1-row track keeps a 1-row
    // thumb (layout.ts:289-292).
    let min_thumb_height = (2.0f64).min(track);
    let thumb_height = min_thumb_height.max(track.min(((track * track) / content).round()));
    let thumb_height = thumb_height as usize;
    let max_scroll_top = content_height.saturating_sub(track_height);
    let max_thumb_top = track_height - thumb_height;
    let thumb_offset = if max_scroll_top == 0 {
        0
    } else {
        ((scroll_top as f64 / max_scroll_top as f64) * max_thumb_top as f64).round() as isize
    };
    let column = layout_box.rect.x + layout_box.rect.width as isize - 1;
    if column < layout_box.clip.x || column >= layout_box.clip.x + layout_box.clip.width as isize {
        return None;
    }

    Some(ScrollbarGeometry {
        column,
        track_top: layout_box.rect.y,
        track_height,
        thumb_top: layout_box.rect.y + thumb_offset,
        thumb_height,
        max_scroll_top,
    })
}

/// `paintScrollbar` (layout.ts:309-330 @ 9841914, 457ae8c79): the full
/// track renders; thumb rows use `█` while the scrollbar is active
/// (hover/drag) and `┃` otherwise, other rows render the track glyph `│`.
/// `always` reserves an unstyled column, so the underlying background is
/// only preserved for the transient overlay modes.
fn paint_scrollbar(layout_box: &LayoutBox, screen: &mut [String], total_width: usize) {
    let Some(shared) = layout_box.scroll_view.as_ref() else {
        return;
    };
    let (track_style, thumb_style, is_active, scrollbar) = {
        let guard = lock_component(shared);
        let Some(scroll_view) = guard.as_scroll_view() else {
            return;
        };
        (
            scroll_view.scrollbar_track_style.clone(),
            scroll_view.scrollbar_thumb_style.clone(),
            scroll_view.is_scrollbar_active(),
            scroll_view.scrollbar(),
        )
    };
    let Some(geometry) = get_scrollbar_geometry(layout_box, false) else {
        return;
    };

    for offset in 0..geometry.track_height {
        let row = geometry.track_top + offset as isize;
        if row < layout_box.clip.y
            || row >= layout_box.clip.y + layout_box.clip.height as isize
            || row < 0
            || row >= screen.len() as isize
        {
            continue;
        }
        let is_thumb =
            row >= geometry.thumb_top && row < geometry.thumb_top + geometry.thumb_height as isize;
        let replacement = if is_thumb {
            thumb_style(if is_active { "█" } else { "┃" })
        } else {
            track_style("│")
        };
        screen[row as usize] = replace_scrollbar_cell(
            &screen[row as usize],
            geometry.column as usize,
            total_width,
            &replacement,
            scrollbar != ScrollbarMode::Always,
        );
    }
}

/// Strip a leading run of OSC 133 prompt-zone markers
/// (`OSC133_ZONE_PREFIX`, layout.ts:8): `^(?:\x1b\]133;[ABC](?:\x07|\x1b\\))+`.
fn strip_osc133_zone_prefix(line: &str) -> &str {
    let mut rest = line;
    while let Some(after_prefix) = rest.strip_prefix("\x1b]133;") {
        let Some(&zone) = after_prefix.as_bytes().first() else {
            break;
        };
        if !matches!(zone, b'A' | b'B' | b'C') {
            break;
        }
        let after_zone = &after_prefix[1..];
        if let Some(stripped) = after_zone.strip_prefix('\x07') {
            rest = stripped;
        } else if let Some(stripped) = after_zone.strip_prefix("\x1b\\") {
            rest = stripped;
        } else {
            break;
        }
    }
    rest
}

/// The cropped remainder of the image line at `image_row` when it sticks out
/// below the scroll top (`hiddenRows < rows`, layout.ts:338-345); `None` for
/// non-image lines and images that end above the viewport.
fn residual_kitty_crop(
    image_line: &str,
    image_row: usize,
    scroll_top: usize,
    layout_box: &LayoutBox,
) -> Option<String> {
    let metadata = get_kitty_image_metadata(image_line)?;
    let hidden_rows = (scroll_top - image_row) as u32;
    if hidden_rows >= metadata.rows {
        return None;
    }
    let visible_rows = (layout_box.rect.height as u32).min(metadata.rows - hidden_rows);
    Some(crop_kitty_image_line(image_line, hidden_rows, visible_rows))
}

/// `paintBox` (layout.ts:304-351): paint leaf lines (row-clamped to clip and
/// screen), then children, then the scroll-top residual image crop hook, and
/// finally the scrollbar on top.
fn paint_box(layout_box: &LayoutBox, screen: &mut [String], total_width: usize) {
    if let Some(lines) = &layout_box.lines {
        let offset = layout_box.line_offset;
        let first_row = layout_box.rect.y.max(layout_box.clip.y).max(0);
        let last_row = (layout_box.rect.y + layout_box.rect.height as isize)
            .min(layout_box.clip.y + layout_box.clip.height as isize)
            .min(screen.len() as isize);
        for row in first_row..last_row {
            // row >= rect.y (first_row), so the content row is non-negative.
            // A virtualized transcript document only carries the viewport
            // window: rows before it are simply not painted (they are
            // outside the clip anyway).
            let content_row = (row - layout_box.rect.y) as usize;
            let Some(relative) = content_row.checked_sub(layout_box.line_window_start) else {
                continue;
            };
            let index = offset + relative;
            let Some(source_line) = lines.get(index) else {
                continue;
            };
            let mut line = strip_osc133_zone_prefix(source_line);
            let cropped_line;
            if let Some(metadata) = get_kitty_image_metadata(line) {
                // Kitty bottom crop (layout.ts:313-318).
                let clip_bottom = (screen.len() as isize)
                    .min(layout_box.clip.y + layout_box.clip.height as isize);
                let visible_rows = metadata.rows.min((clip_bottom - row).max(0) as u32);
                if visible_rows < metadata.rows {
                    cropped_line = crop_kitty_image_line(line, 0, visible_rows);
                    line = &cropped_line;
                }
            }
            // Fast path (layout.ts:319-328): a full-width box painting onto an
            // untouched row uses the source line directly; compositing would
            // re-segment the row every frame.
            if layout_box.rect.x == 0
                && layout_box.rect.width >= total_width
                && (is_image_line(line) || screen[row as usize].is_empty())
            {
                screen[row as usize] = line.to_string();
            } else {
                screen[row as usize] = composite_tui_line(
                    &screen[row as usize],
                    line,
                    layout_box.rect.x as i32,
                    layout_box.rect.width as i32,
                    total_width as i32,
                );
            }
        }
    }
    for child in &layout_box.children {
        paint_box(child, screen, total_width);
    }

    // Scroll-top residual image crop (layout.ts:333-348): when scrolled down,
    // the last image line above the viewport still sticks out at the top row;
    // repaint it cropped to its visible remainder. The rows above the
    // viewport come from the scroll content (whole array, or the transcript
    // document's on-demand rows — V19-06 FR-C R4).
    if let (Some(shared), Some(content)) =
        (layout_box.scroll_view.as_ref(), layout_box.scroll_content())
    {
        let scroll_top = {
            let guard = lock_component(shared);
            match guard.as_scroll_view() {
                Some(scroll_view) => scroll_view.scroll_top(),
                None => 0,
            }
        };
        if scroll_top > 0 && layout_box.rect.height > 0 {
            let mut residual: Option<String> = None;
            match &content {
                ScrollContent::Full(content_lines) => {
                    for image_row in (0..scroll_top).rev() {
                        let image_line = content_lines
                            .get(image_row)
                            .map(String::as_str)
                            .unwrap_or("");
                        if let Some(cropped) =
                            residual_kitty_crop(image_line, image_row, scroll_top, layout_box)
                        {
                            residual = Some(cropped);
                        }
                        if !image_line.is_empty() {
                            break;
                        }
                    }
                }
                ScrollContent::Virtual { .. } => {
                    for image_row in (0..scroll_top).rev() {
                        let image_line = content.line(image_row).unwrap_or_default();
                        if let Some(cropped) =
                            residual_kitty_crop(&image_line, image_row, scroll_top, layout_box)
                        {
                            residual = Some(cropped);
                        }
                        if !image_line.is_empty() {
                            break;
                        }
                    }
                }
            }
            if let Some(cropped) = residual
                && layout_box.rect.x == 0
                && layout_box.rect.width >= total_width
                && layout_box.rect.y >= 0
                && (layout_box.rect.y as usize) < screen.len()
            {
                screen[layout_box.rect.y as usize] = cropped;
            }
        }
    }

    paint_scrollbar(layout_box, screen, total_width);
}

/// `renderLayoutFrame` (layout.ts:353-382) with a frame-local render cache:
/// every call starts from an empty cross-frame cache, which keeps the
/// upstream one-frame semantics for callers that do not own a
/// [`LayoutRenderCache`] (tests, one-shot renders).
pub fn render_layout_frame(
    root: &SharedComponent,
    width: usize,
    height: usize,
    request_render: RenderHandle,
) -> LayoutFrame {
    let mut cache = LayoutRenderCache::new();
    render_layout_frame_cached(root, width, height, request_render, &mut cache)
}

/// `renderLayoutFrame` over a renderer-owned cross-frame cache (V19-06
/// FR-A): trusted components whose width and revision did not change are
/// served from `cache` without a `render()` call.
pub fn render_layout_frame_cached(
    root: &SharedComponent,
    width: usize,
    height: usize,
    request_render: RenderHandle,
    cache: &mut LayoutRenderCache,
) -> LayoutFrame {
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    let mut context = LayoutContext {
        viewport: LayoutViewport {
            width: safe_width,
            height: safe_height,
        },
        render_cache: HashMap::new(),
        cache,
        request_render,
        primary_scroll_view: None,
    };
    let root_clip = LayoutRect {
        x: 0,
        y: 0,
        width: safe_width,
        height: safe_height,
    };
    let root_box = layout_component(
        &mut context,
        root,
        0,
        0,
        safe_width,
        Some(safe_height),
        &root_clip,
    );
    let mut lines = vec![String::new(); safe_height];
    paint_box(&root_box, &mut lines, safe_width);
    LayoutFrame {
        root: root_box,
        width: safe_width,
        height: safe_height,
        lines,
        primary_scroll_view: context.primary_scroll_view,
    }
}

/// `containsPoint` (layout.ts:384-386).
fn contains_point(rect: &LayoutRect, x: isize, y: isize) -> bool {
    x >= rect.x
        && x < rect.x + rect.width as isize
        && y >= rect.y
        && y < rect.y + rect.height as isize
}

/// `getScrollViewBox` (layout.ts:388-398): find the box whose scroll view IS
/// `scroll_view` (`Arc::ptr_eq`, upstream `===`).
pub fn get_scroll_view_box<'a>(
    frame: &'a LayoutFrame,
    scroll_view: &SharedComponent,
) -> Option<&'a LayoutBox> {
    fn visit<'a>(layout_box: &'a LayoutBox, target: &SharedComponent) -> Option<&'a LayoutBox> {
        if layout_box
            .scroll_view
            .as_ref()
            .is_some_and(|shared| Arc::ptr_eq(shared, target))
        {
            return Some(layout_box);
        }
        for child in &layout_box.children {
            if let Some(found) = visit(child, target) {
                return Some(found);
            }
        }
        None
    }
    visit(&frame.root, scroll_view)
}

/// `getScrollViewsAt` (layout.ts:400-410): clip-pruned, rect hit-tested,
/// deepest first (stable sort keeps DFS order within a depth, like JS).
pub fn get_scroll_views_at(frame: &LayoutFrame, x: isize, y: isize) -> Vec<SharedComponent> {
    fn visit(
        layout_box: &LayoutBox,
        x: isize,
        y: isize,
        depth: usize,
        result: &mut Vec<(SharedComponent, usize)>,
    ) {
        if !contains_point(&layout_box.clip, x, y) {
            return;
        }
        if let Some(shared) = layout_box.scroll_view.as_ref()
            && contains_point(&layout_box.rect, x, y)
        {
            result.push((shared.clone(), depth));
        }
        for child in &layout_box.children {
            visit(child, x, y, depth + 1, result);
        }
    }
    let mut result = Vec::new();
    visit(&frame.root, x, y, 0, &mut result);
    // Stable sort by depth descending (JS `Array.prototype.sort` is stable
    // too), keeping DFS order within a depth.
    result.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    result.into_iter().map(|(shared, _)| shared).collect()
}

/// `getLayoutBoxesAt` (layout.ts:415-424 @ 9841914, 71026970a): the visual
/// hit path from the deepest component to the layout root. A box is on the
/// path when its CLIP contains the point (ancestors whose clip contains the
/// point are included); ordering is layer descending then depth descending
/// (stable sort keeps DFS order within equal keys, like JS).
pub fn get_layout_boxes_at(frame: &LayoutFrame, x: isize, y: isize) -> Vec<&LayoutBox> {
    fn visit<'a>(
        layout_box: &'a LayoutBox,
        x: isize,
        y: isize,
        depth: usize,
        result: &mut Vec<(&'a LayoutBox, i32, usize)>,
    ) {
        if !contains_point(&layout_box.clip, x, y) {
            return;
        }
        result.push((layout_box, layout_box.layer, depth));
        for child in &layout_box.children {
            visit(child, x, y, depth + 1, result);
        }
    }
    let mut result = Vec::new();
    visit(&frame.root, x, y, 0, &mut result);
    result.sort_by_key(|entry| (std::cmp::Reverse(entry.1), std::cmp::Reverse(entry.2)));
    result
        .into_iter()
        .map(|(layout_box, _, _)| layout_box)
        .collect()
}
