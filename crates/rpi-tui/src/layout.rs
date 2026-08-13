//! Port of `packages/tui/src/layout.ts` @ pi 4181f66.
//!
//! The layout engine: walks the component tree via `Component::layout_node`
//! (the T30 replacement for upstream's `LAYOUT_NODE` symbol protocol),
//! measures stack/scroll containers, and paints the frame into a line buffer.
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
use std::sync::Arc;

use crate::components::scroll_view::ScrollbarStyleFn;
use crate::components::stack::{allocate_stack_sizes, visible_stack_entries};
use crate::layout_node::{Basis, LayoutNode, LayoutViewport, StackAlign, StackKind};
use crate::terminal_image::{crop_kitty_image_line, get_kitty_image_metadata, is_image_line};
use crate::tui::{
    composite_tui_line, lock_component, RenderHandle, SharedComponent, CURSOR_MARKER,
};
use crate::utils::{extract_ansi_code, get_grapheme_cell_range, slice_by_column, visible_width};

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
    pub scroll_content_lines: Option<Arc<[String]>>,
    /// Kept for parity; always 0 (layout.ts:27).
    pub layer: i32,
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
struct LayoutContext {
    viewport: LayoutViewport,
    /// Keyed by `Arc::as_ptr` address, then by width (see header note).
    render_cache: HashMap<usize, HashMap<usize, Arc<[String]>>>,
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
fn render_cached(
    context: &mut LayoutContext,
    component: &SharedComponent,
    width: usize,
) -> Arc<[String]> {
    let safe_width = width.max(1);
    let key = Arc::as_ptr(component) as usize;
    let widths = context.render_cache.entry(key).or_default();
    if let Some(lines) = widths.get(&safe_width) {
        return lines.clone();
    }
    let lines: Arc<[String]> = lock_component(component).render(safe_width).into();
    widths.insert(safe_width, lines.clone());
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
                {
                    if cursor_line >= allocated_height {
                        line_offset = cursor_line - allocated_height + 1;
                    }
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
                        layer: 0,
                    }
                }
            }
        }
    }
}

/// `styleScrollbarCell` (layout.ts:243-264): apply the scrollbar style to the
/// single terminal cell at `column` WITHOUT replacing the cell's text —
/// wide graphemes spanning the column are styled whole.
fn style_scrollbar_cell(
    line: &str,
    column: usize,
    total_width: usize,
    style: &ScrollbarStyleFn,
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

    // ANSI prefixes stay OUTSIDE the styled text (layout.ts:253-260).
    let mut target_prefix = String::new();
    let mut target_index = 0;
    while target_index < target.len() {
        let Some(ansi) = extract_ansi_code(&target, target_index) else {
            break;
        };
        target_prefix.push_str(ansi.code);
        target_index += ansi.length;
    }
    let target_text = &target[target_index..];
    let owned_target;
    let target_text = if target_text.is_empty() {
        owned_target = " ".repeat(end - start);
        owned_target.as_str()
    } else {
        target_text
    };
    let before_padding = " ".repeat(start.saturating_sub(visible_width(&before)));
    format!(
        "{before}{before_padding}{target_prefix}{}{after}",
        style(target_text)
    )
}

/// `getScrollbarGeometry` (layout.ts:266-291).
pub fn get_scrollbar_geometry(layout_box: &LayoutBox) -> Option<ScrollbarGeometry> {
    let shared = layout_box.scroll_view.as_ref()?;
    let (is_visible, scroll_top) = {
        let guard = lock_component(shared);
        let scroll_view = guard.as_scroll_view()?;
        (scroll_view.is_scrollbar_visible(), scroll_view.scroll_top())
    };
    if !is_visible || layout_box.rect.width == 0 || layout_box.rect.height == 0 {
        return None;
    }

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

    // f64 so content_height == 0 yields +inf and `min(track, inf) == track`,
    // exactly like upstream's division by zero.
    let track = track_height as f64;
    let content = content_height as f64;
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

/// `paintScrollbar` (layout.ts:293-302).
fn paint_scrollbar(layout_box: &LayoutBox, screen: &mut [String], total_width: usize) {
    let Some(geometry) = get_scrollbar_geometry(layout_box) else {
        return;
    };
    let Some(shared) = layout_box.scroll_view.as_ref() else {
        return;
    };
    let style = {
        let guard = lock_component(shared);
        match guard.as_scroll_view() {
            Some(scroll_view) => scroll_view.scrollbar_style.clone(),
            None => return,
        }
    };

    for offset in 0..geometry.thumb_height {
        let row = geometry.thumb_top + offset as isize;
        if row < layout_box.clip.y
            || row >= layout_box.clip.y + layout_box.clip.height as isize
            || row < 0
            || row >= screen.len() as isize
        {
            continue;
        }
        screen[row as usize] = style_scrollbar_cell(
            &screen[row as usize],
            geometry.column as usize,
            total_width,
            &style,
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
            // row >= rect.y (first_row), so the index is non-negative.
            let index = offset + (row - layout_box.rect.y) as usize;
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
    // repaint it cropped to its visible remainder.
    if let (Some(shared), Some(content_lines)) = (
        layout_box.scroll_view.as_ref(),
        layout_box.scroll_content_lines.as_ref(),
    ) {
        let scroll_top = {
            let guard = lock_component(shared);
            match guard.as_scroll_view() {
                Some(scroll_view) => scroll_view.scroll_top(),
                None => 0,
            }
        };
        if scroll_top > 0 && layout_box.rect.height > 0 {
            for image_row in (0..scroll_top).rev() {
                let image_line = content_lines
                    .get(image_row)
                    .map(String::as_str)
                    .unwrap_or("");
                if let Some(metadata) = get_kitty_image_metadata(image_line) {
                    let hidden_rows = (scroll_top - image_row) as u32;
                    if hidden_rows < metadata.rows {
                        let visible_rows =
                            (layout_box.rect.height as u32).min(metadata.rows - hidden_rows);
                        let cropped = crop_kitty_image_line(image_line, hidden_rows, visible_rows);
                        // Upstream writes `screen[box.rect.y]` unconditionally
                        // (JS arrays auto-extend); see the header note.
                        if layout_box.rect.x == 0
                            && layout_box.rect.width >= total_width
                            && layout_box.rect.y >= 0
                            && (layout_box.rect.y as usize) < screen.len()
                        {
                            screen[layout_box.rect.y as usize] = cropped;
                        }
                    }
                    break;
                }
                if !image_line.is_empty() {
                    break;
                }
            }
        }
    }

    paint_scrollbar(layout_box, screen, total_width);
}

/// `renderLayoutFrame` (layout.ts:353-382). The render cache lives exactly
/// one frame.
pub fn render_layout_frame(
    root: &SharedComponent,
    width: usize,
    height: usize,
    request_render: RenderHandle,
) -> LayoutFrame {
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    let mut context = LayoutContext {
        viewport: LayoutViewport {
            width: safe_width,
            height: safe_height,
        },
        render_cache: HashMap::new(),
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
        if let Some(shared) = layout_box.scroll_view.as_ref() {
            if contains_point(&layout_box.rect, x, y) {
                result.push((shared.clone(), depth));
            }
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
