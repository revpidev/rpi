//! Viewport virtualization contract for transcript documents (V19-06, ADR-0049
//! decision 4, design §5.2).
//!
//! A transcript document is a vertical sequence of independently renderable
//! blocks (messages, spacers, tool components). The layout engine asks it for
//! a height index instead of rendering every off-screen block each frame: the
//! document measures a block once per (identity, revision, width), keeps the
//! rendered lines of a bounded working set, and materializes only the rows a
//! consumer asks for.
//!
//! Identity protocol (why this is safe across frames):
//!
//! - A block is *trusted* when [`Component::cache_instance_id`] and
//!   [`Component::cache_revision`] both return `Some`. The instance id is
//!   drawn from a process-wide counter and never reused, so a replaced block
//!   can never validate against the cached lines of the block it displaced,
//!   even when the allocator hands the new block the same address (the ABA
//!   case). The revision is monotonic per instance (V19-04 contract).
//! - Untrusted blocks are re-rendered on every sync (the local full-render
//!   fallback) and counted in [`TranscriptStats::fallback_renders`]; they
//!   never disable virtualization for the rest of the document.
//!
//! Full-text consumers (scrollbar, OSC 133 prompt jump, search, selection and
//! Kitty residual crops) go through the same contract: [`TranscriptDocument::rows`]
//! materializes a row range on demand and [`TranscriptDocument::materialize`]
//! builds the lazy full-text corpus used by search, cached per width and
//! content stamp and dropped once two syncs go by without a consumer.
//!
//! Known costs (bounded, by design): the first full-text consumer (opening a
//! search, copying a whole history) renders every evicted block once and
//! retains the whole history for up to two frames before the LRU caps apply
//! again; while a search is open, any untrusted block (re-rendered every
//! frame) changes the content stamp every frame, so the corpus is
//! re-concatenated per frame from the reused per-block segments (an O(rows)
//! copy, never a re-render of unchanged trusted blocks).

use std::ops::Range;
use std::sync::Arc;

/// Retained-lines capacity of a transcript document: blocks beyond the
/// working set (V19-06 FR-E R2).
pub const MAX_RETAINED_CHILDREN: usize = 256;
/// Retained-lines capacity of a transcript document: rows beyond the working
/// set (a bounded amount of rendered text — about one to two MiB; the working
/// set itself is never evicted).
pub const MAX_RETAINED_LINES: usize = 16_384;

/// Result of [`TranscriptDocument::sync`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSync {
    /// Content stamp: changes whenever any block's lines or the block
    /// structure changed, or the width changed. Equal stamps mean equal
    /// content at the same width.
    pub stamp: u64,
    /// Total content height in rows at the synced width.
    pub height: usize,
}

/// Counters exposed for tests and the performance report (V19-06 FR-B R4,
/// FR-E R4). All counters are cumulative for the document lifetime.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptStats {
    /// Number of `sync` calls.
    pub syncs: u64,
    /// Blocks (flattened, nested documents included) at the last sync.
    pub blocks: usize,
    /// Trusted blocks at the last sync.
    pub trusted_blocks: usize,
    /// Blocks that fell back to the per-frame local render (untrusted).
    pub fallback_blocks: usize,
    /// Trusted slots validated without a render.
    pub trusted_hits: u64,
    /// Renders of trusted blocks whose identity or revision changed (or that
    /// were unknown at the width).
    pub trusted_renders: u64,
    /// Renders of untrusted blocks (the bounded local fallback).
    pub fallback_renders: u64,
    /// Renders served on demand for a row range, a single line or the search
    /// corpus (a block scrolled into view whose lines were evicted).
    pub on_demand_renders: u64,
    /// Evictions from the bounded retained-lines set.
    pub evictions: u64,
    /// Rebuilds of the materialized full-text corpus.
    pub corpus_rebuilds: u64,
    /// Retained children at the last sync (bounded working set).
    pub retained_children: usize,
    /// Retained lines at the last sync (bounded working set).
    pub retained_lines: usize,
}

/// A document that can be measured through a height index instead of a full
/// render (V19-06 FR-B, design §5.2).
///
/// All methods take `&self`; implementations use interior mutability exactly
/// like `Component::render`. Callers must call [`Self::sync`] once per frame
/// at the frame's width before asking for ranges; the row accessors also
/// work outside a frame (they render what they need on demand), which is how
/// selection copy and prompt jumps read rows between frames.
///
/// The document may be restructured between a sync and a read: the
/// application shares the chat container with the session thread through an
/// `Arc<Mutex<..>>`, so a `/clear`, a theme rebuild or an append can land
/// while the driver thread is mid-frame (between the layout's `sync` and its
/// `rows`, or before the residual image crop) or between frames. The row
/// accessors ([`Self::rows`], [`Self::line`], [`Self::render_range`],
/// [`Self::materialize`]) follow the children — a restructured document is
/// re-measured first (the next sync, moved ahead of the read), so the rows
/// returned are always the current content's. The index accessors
/// ([`Self::revision_at`], [`Self::height_at`], [`Self::visible_range`],
/// [`Self::prompt_rows`], [`Self::block_row`]) answer "unmeasured" until the
/// next sync (`None`, `0..0`, empty, the content end) rather than report a
/// displaced block; a row window computed from them clamps to the content.
pub trait TranscriptDocument {
    /// Brings the height index up to date for `width`: measures every block
    /// whose identity or revision changed (and every untrusted block), then
    /// returns the content stamp and the total height.
    fn sync(&self, width: usize) -> TranscriptSync;

    /// Total content height in rows at the last synced width.
    fn total_height(&self) -> usize;

    /// Number of flattened blocks (nested documents expand to their blocks).
    fn child_count(&self) -> usize;

    /// Revision of block `index`: the block's own revision for trusted
    /// opaque blocks, the nested document stamp for nested ones, `None` for
    /// untrusted blocks.
    fn revision_at(&self, index: usize) -> Option<u64>;

    /// Height of block `index` at the last synced width; `None` when the
    /// index is out of range.
    fn height_at(&self, index: usize) -> Option<usize>;

    /// Flattened block indices whose rows intersect the window
    /// `[scroll_top - overscan, scroll_top + viewport + overscan)` (clamped to
    /// the content).
    fn visible_range(&self, scroll_top: usize, viewport: usize, overscan: usize) -> Range<usize>;

    /// Full lines of the flattened blocks `children`, concatenated.
    fn render_range(&self, children: Range<usize>, width: usize) -> Arc<[String]>;

    /// The content rows `rows` (clamped to the content), materialized on
    /// demand.
    fn rows(&self, rows: Range<usize>, width: usize) -> Arc<[String]>;

    /// One content row, or `None` past the end.
    fn line(&self, row: usize, width: usize) -> Option<String>;

    /// The full-text corpus at `width`: built lazily, cached per
    /// (width, content stamp) and released when a frame goes by without a
    /// consumer asking for it.
    fn materialize(&self, width: usize) -> Arc<[String]>;

    /// Rows (in content coordinates) whose line starts an OSC 133 prompt zone
    /// (`OSC133_PROMPT_START`), ascending.
    fn prompt_rows(&self) -> Vec<usize>;

    /// First content row of flattened block `index` (`child_count()` gives
    /// the content end).
    fn block_row(&self, index: usize) -> usize;

    /// Cumulative counters.
    fn stats(&self) -> TranscriptStats;
}

/// `OSC133_PROMPT_START` (tui-alt-screen.ts:56):
/// `/^\x1b\]133;A(?:\x07|\x1b\\)/`. Shared by the alt-screen prompt jump
/// and the transcript prompt index, so both recognize the same lines.
pub fn is_osc133_prompt_start(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("\x1b]133;A") else {
        return false;
    };
    rest.starts_with('\x07') || rest.starts_with("\x1b\\")
}

/// Row offsets of the prompt-start lines in `lines` (the per-block prompt
/// index maintained with the height index).
pub(crate) fn prompt_offsets(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_osc133_prompt_start(line))
        .map(|(row, _)| row as u32)
        .collect()
}

/// Clamp a half-open row window to `[0, height)` for a viewport that starts
/// at `scroll_top` and is `viewport` rows tall, extended by `overscan` rows on
/// both sides.
pub fn clamp_window(
    scroll_top: usize,
    viewport: usize,
    overscan: usize,
    height: usize,
) -> Range<usize> {
    let start = scroll_top.saturating_sub(overscan).min(height);
    let end = scroll_top
        .saturating_add(viewport)
        .saturating_add(overscan)
        .min(height)
        .max(start);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_start_requires_the_zone_marker_at_the_line_start() {
        assert!(is_osc133_prompt_start("\x1b]133;A\x07prompt"));
        assert!(is_osc133_prompt_start("\x1b]133;A\x1b\\prompt"));
        assert!(!is_osc133_prompt_start("text\x1b]133;A\x07"));
        assert!(!is_osc133_prompt_start("\x1b]133;B\x07"));
        assert_eq!(
            prompt_offsets(&[
                "\x1b]133;A\x07a".to_string(),
                "b".to_string(),
                "\x1b]133;A\x1b\\c".to_string()
            ]),
            vec![0, 2]
        );
    }

    #[test]
    fn window_is_clamped_to_the_content() {
        assert_eq!(clamp_window(0, 10, 2, 30), 0..12);
        assert_eq!(clamp_window(20, 10, 2, 30), 18..30);
        assert_eq!(clamp_window(5, 10, 50, 30), 0..30);
        assert_eq!(clamp_window(0, 10, 2, 0), 0..0);
    }
}
