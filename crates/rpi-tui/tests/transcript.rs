//! V19-06 viewport virtualization: the transcript document contract and its
//! layout integration, checked against the unoptimized full-render path.
//!
//! Every equivalence assertion renders the SAME container twice — once as an
//! opaque scroll child (the pre-V19-06 full-render oracle) and once as a
//! transcript document — and compares bytes, heights and coordinates. The
//! identity protocol (instance id + revision), the bounded fallback for
//! untrusted blocks and the cross-frame layout cache are asserted through
//! render counters.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_tui::components::scroll_view::{Follow, ScrollView, ScrollViewOptions, ScrollbarMode};
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::stack::{StackChild, StackOptions};
use rpi_tui::components::text::Text;
use rpi_tui::components::v_stack::VStack;
use rpi_tui::layout::{
    TRANSCRIPT_OVERSCAN_ROWS, get_scroll_view_box, get_scrollbar_geometry,
    render_layout_frame_cached, with_transcript,
};
use rpi_tui::layout_cache::LayoutRenderCache;
use rpi_tui::terminal_image::{
    KittyEncodeOptions, KittyImageMetadata, encode_kitty, register_kitty_image_metadata,
};
use rpi_tui::transcript::{
    MAX_RETAINED_CHILDREN, MAX_RETAINED_LINES, TranscriptDocument, is_osc133_prompt_start,
};
use rpi_tui::tui::{
    Component, Container, RenderHandle, SharedComponent, next_cache_instance_id, shared_component,
    shared_component_from_boxed,
};

const OSC133_PROMPT_START: &str = "\x1b]133;A\x07";

fn noop_render_handle() -> RenderHandle {
    RenderHandle::new(|| {})
}

fn lock<T>(shared: &Arc<Mutex<T>>) -> std::sync::MutexGuard<'_, T> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A trusted block: process-unique instance id + monotonic revision, with a
/// render counter (the V19-04/V19-06 identity protocol).
struct TrustedBlock {
    renders: Arc<AtomicU64>,
    revision: Arc<AtomicU64>,
    id: u64,
    lines: Vec<String>,
}

impl Component for TrustedBlock {
    fn render(&self, _width: usize) -> Vec<String> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        self.lines.clone()
    }
    fn cache_revision(&self) -> Option<u64> {
        Some(self.revision.load(Ordering::Relaxed))
    }
    fn cache_instance_id(&self) -> Option<u64> {
        Some(self.id)
    }
}

fn trusted(lines: Vec<String>) -> (Box<dyn Component>, Arc<AtomicU64>, Arc<AtomicU64>) {
    let renders = Arc::new(AtomicU64::new(0));
    let revision = Arc::new(AtomicU64::new(0));
    (
        Box::new(TrustedBlock {
            renders: Arc::clone(&renders),
            revision: Arc::clone(&revision),
            id: next_cache_instance_id(),
            lines,
        }),
        renders,
        revision,
    )
}

/// An untrusted block (no revision protocol): the local per-frame fallback.
struct UntrustedBlock {
    renders: Arc<AtomicU64>,
    lines: Vec<String>,
}

impl Component for UntrustedBlock {
    fn render(&self, _width: usize) -> Vec<String> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        self.lines.clone()
    }
}

fn untrusted(lines: Vec<String>) -> (Box<dyn Component>, Arc<AtomicU64>) {
    let renders = Arc::new(AtomicU64::new(0));
    (
        Box::new(UntrustedBlock {
            renders: Arc::clone(&renders),
            lines,
        }),
        renders,
    )
}

/// The scroll child as the application mounts it: a wrapper holding the
/// container behind an `Arc<Mutex<..>>`. `expose` decides whether the wrapper
/// is a transcript document (the V19-06 path) or an opaque leaf (the
/// full-render oracle).
struct DocWrapper {
    container: Arc<Mutex<Container>>,
    expose: bool,
    full_renders: Arc<AtomicU64>,
}

impl Component for DocWrapper {
    fn render(&self, width: usize) -> Vec<String> {
        self.full_renders.fetch_add(1, Ordering::Relaxed);
        lock(&self.container).render(width)
    }
    fn invalidate(&mut self) {
        lock(&self.container).invalidate();
    }
    fn as_transcript_document(&self) -> Option<&dyn TranscriptDocument> {
        self.expose.then_some(self)
    }
}

impl TranscriptDocument for DocWrapper {
    fn sync(&self, width: usize) -> rpi_tui::transcript::TranscriptSync {
        lock(&self.container).sync(width)
    }
    fn total_height(&self) -> usize {
        lock(&self.container).total_height()
    }
    fn child_count(&self) -> usize {
        lock(&self.container).child_count()
    }
    fn revision_at(&self, index: usize) -> Option<u64> {
        lock(&self.container).revision_at(index)
    }
    fn height_at(&self, index: usize) -> Option<usize> {
        lock(&self.container).height_at(index)
    }
    fn visible_range(&self, scroll_top: usize, viewport: usize, overscan: usize) -> Range<usize> {
        lock(&self.container).visible_range(scroll_top, viewport, overscan)
    }
    fn render_range(&self, children: Range<usize>, width: usize) -> Arc<[String]> {
        lock(&self.container).render_range(children, width)
    }
    fn rows(&self, rows: Range<usize>, width: usize) -> Arc<[String]> {
        lock(&self.container).rows(rows, width)
    }
    fn line(&self, row: usize, width: usize) -> Option<String> {
        lock(&self.container).line(row, width)
    }
    fn materialize(&self, width: usize) -> Arc<[String]> {
        lock(&self.container).materialize(width)
    }
    fn prompt_rows(&self) -> Vec<usize> {
        lock(&self.container).prompt_rows()
    }
    fn block_row(&self, index: usize) -> usize {
        lock(&self.container).block_row(index)
    }
    fn stats(&self) -> rpi_tui::transcript::TranscriptStats {
        lock(&self.container).stats()
    }
}

/// A nested document (the application's `SharedChild<Container>` regions
/// inside the document container).
struct NestedDoc {
    container: Arc<Mutex<Container>>,
}

impl Component for NestedDoc {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.container).render(width)
    }
    fn invalidate(&mut self) {
        lock(&self.container).invalidate();
    }
    fn as_transcript_document(&self) -> Option<&dyn TranscriptDocument> {
        Some(self)
    }
}

impl TranscriptDocument for NestedDoc {
    fn sync(&self, width: usize) -> rpi_tui::transcript::TranscriptSync {
        lock(&self.container).sync(width)
    }
    fn total_height(&self) -> usize {
        lock(&self.container).total_height()
    }
    fn child_count(&self) -> usize {
        lock(&self.container).child_count()
    }
    fn revision_at(&self, index: usize) -> Option<u64> {
        lock(&self.container).revision_at(index)
    }
    fn height_at(&self, index: usize) -> Option<usize> {
        lock(&self.container).height_at(index)
    }
    fn visible_range(&self, scroll_top: usize, viewport: usize, overscan: usize) -> Range<usize> {
        lock(&self.container).visible_range(scroll_top, viewport, overscan)
    }
    fn render_range(&self, children: Range<usize>, width: usize) -> Arc<[String]> {
        lock(&self.container).render_range(children, width)
    }
    fn rows(&self, rows: Range<usize>, width: usize) -> Arc<[String]> {
        lock(&self.container).rows(rows, width)
    }
    fn line(&self, row: usize, width: usize) -> Option<String> {
        lock(&self.container).line(row, width)
    }
    fn materialize(&self, width: usize) -> Arc<[String]> {
        lock(&self.container).materialize(width)
    }
    fn prompt_rows(&self) -> Vec<usize> {
        lock(&self.container).prompt_rows()
    }
    fn block_row(&self, index: usize) -> usize {
        lock(&self.container).block_row(index)
    }
    fn stats(&self) -> rpi_tui::transcript::TranscriptStats {
        lock(&self.container).stats()
    }
}

struct Fixture {
    container: Arc<Mutex<Container>>,
    /// (child, renders, revision) of every trusted block, in push order.
    trusted: Vec<(Arc<AtomicU64>, Arc<AtomicU64>)>,
    untrusted_renders: Vec<Arc<AtomicU64>>,
}

/// A document shaped like a real transcript: prompt-start lines, trusted
/// message blocks, an untrusted block, spacers, a nested container and a
/// Kitty image line.
fn fixture() -> Fixture {
    let container = Arc::new(Mutex::new(Container::new()));
    let mut trusted_blocks = Vec::new();
    let mut untrusted_renders = Vec::new();
    let (block, renders, revision) = trusted(vec![
        format!("{OSC133_PROMPT_START}question one"),
        "answered in **bold**".to_string(),
    ]);
    lock(&container).children.push(block);
    trusted_blocks.push((renders, revision));
    lock(&container).children.push(Box::new(Spacer::new(1)));
    let (block, renders, revision) =
        trusted((0..30).map(|index| format!("answer {index}")).collect());
    lock(&container).children.push(block);
    trusted_blocks.push((renders, revision));
    let (block, renders) = untrusted(vec!["untrusted status line".to_string()]);
    lock(&container).children.push(block);
    untrusted_renders.push(renders);
    let image_id = 4_242u32;
    let image_line = encode_kitty(
        "AAAA",
        &KittyEncodeOptions {
            columns: Some(2),
            rows: Some(3),
            image_id: Some(image_id),
            move_cursor: Some(false),
        },
    );
    register_kitty_image_metadata(KittyImageMetadata {
        image_id,
        columns: 2,
        rows: 3,
        width_px: 100.0,
        height_px: 100.0,
    });
    let (block, renders, revision) = trusted(vec![
        "before the image".to_string(),
        image_line,
        String::new(),
        String::new(),
        "after the image".to_string(),
    ]);
    lock(&container).children.push(block);
    trusted_blocks.push((renders, revision));
    // Nested document: a header region and a chat region.
    let nested = Arc::new(Mutex::new(Container::new()));
    lock(&nested)
        .children
        .push(Box::new(Text::new("header line", 0, 0, None)));
    let (block, renders, revision) = trusted(vec![
        format!("{OSC133_PROMPT_START}question two"),
        "short answer".to_string(),
    ]);
    lock(&nested).children.push(block);
    trusted_blocks.push((renders, revision));
    lock(&container)
        .children
        .push(Box::new(NestedDoc { container: nested }));
    Fixture {
        container,
        trusted: trusted_blocks,
        untrusted_renders,
    }
}

fn scroll_root(
    child: SharedComponent,
    scrollbar: ScrollbarMode,
    follow: Follow,
) -> SharedComponent {
    shared_component(ScrollView::new(
        child,
        ScrollViewOptions {
            follow,
            primary: true,
            scrollbar,
            ..ScrollViewOptions::default()
        },
    ))
}

fn frame(
    root: &SharedComponent,
    width: usize,
    height: usize,
    cache: &mut LayoutRenderCache,
) -> rpi_tui::layout::LayoutFrame {
    render_layout_frame_cached(root, width, height, noop_render_handle(), cache)
}

fn total_renders(fx: &Fixture) -> u64 {
    fx.trusted
        .iter()
        .map(|(renders, _)| renders.load(Ordering::Relaxed))
        .sum()
}

fn visible(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            rpi_test_support::vt::strip_ansi(line)
                .trim_end()
                .to_string()
        })
        .collect()
}

fn with_scroll_view<R>(shared: &SharedComponent, f: impl FnOnce(&ScrollView) -> R) -> R {
    let guard = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.as_scroll_view().expect("scroll view"))
}

/// The oracle and the virtual scroll view over the same container.
fn pair(
    fx: &Fixture,
    scrollbar: ScrollbarMode,
    follow: Follow,
) -> (SharedComponent, SharedComponent) {
    let oracle = scroll_root(
        shared_component(DocWrapper {
            container: Arc::clone(&fx.container),
            expose: false,
            full_renders: Arc::new(AtomicU64::new(0)),
        }),
        scrollbar,
        follow,
    );
    let virtual_ = scroll_root(
        shared_component(DocWrapper {
            container: Arc::clone(&fx.container),
            expose: true,
            full_renders: Arc::new(AtomicU64::new(0)),
        }),
        scrollbar,
        follow,
    );
    (oracle, virtual_)
}

/// Scrolls both views to `scroll_top` and renders one frame each.
fn frames_at(
    oracle: &SharedComponent,
    virtual_: &SharedComponent,
    scroll_top: i64,
    width: usize,
    height: usize,
    oracle_cache: &mut LayoutRenderCache,
    virtual_cache: &mut LayoutRenderCache,
) -> (rpi_tui::layout::LayoutFrame, rpi_tui::layout::LayoutFrame) {
    with_scroll_view(oracle, |view| view.scroll_to(scroll_top));
    with_scroll_view(virtual_, |view| view.scroll_to(scroll_top));
    (
        frame(oracle, width, height, oracle_cache),
        frame(virtual_, width, height, virtual_cache),
    )
}

// ---- FR-B R1/R3: heights, rows and frames equal the full render ----------

#[test]
fn virtual_frames_match_the_full_render_oracle() {
    let fx = fixture();
    for (scrollbar, follow, width, height) in [
        (ScrollbarMode::Hidden, Follow::End, 40, 8),
        (ScrollbarMode::Auto, Follow::None, 30, 5),
        (ScrollbarMode::Always, Follow::End, 24, 12),
    ] {
        let (oracle, virtual_) = pair(&fx, scrollbar, follow);
        let mut oracle_cache = LayoutRenderCache::new();
        let mut virtual_cache = LayoutRenderCache::new();
        // First frame: both measure everything.
        let (first_oracle, first_virtual) = frames_at(
            &oracle,
            &virtual_,
            0,
            width,
            height,
            &mut oracle_cache,
            &mut virtual_cache,
        );
        assert_eq!(
            first_oracle.lines, first_virtual.lines,
            "{scrollbar:?}/{follow:?}/{width}x{height}"
        );
        let content_height = first_oracle
            .root
            .children
            .first()
            .map(|child| child.rect.height)
            .unwrap_or(0);
        assert_eq!(content_height, 2 + 1 + 30 + 1 + 5 + 3);
        for scroll_top in 0..=content_height {
            let (oracle_frame, virtual_frame) = frames_at(
                &oracle,
                &virtual_,
                scroll_top as i64,
                width,
                height,
                &mut oracle_cache,
                &mut virtual_cache,
            );
            assert_eq!(
                oracle_frame.lines, virtual_frame.lines,
                "scroll_top {scroll_top} ({scrollbar:?}/{follow:?}/{width}x{height})"
            );
            assert_eq!(
                get_scrollbar_geometry(scroll_box(&oracle_frame), false),
                get_scrollbar_geometry(scroll_box(&virtual_frame), false),
                "scrollbar geometry at scroll_top {scroll_top}"
            );
            // Heights are the full render's heights.
            let oracle_box = scroll_box(&oracle_frame);
            let virtual_box = scroll_box(&virtual_frame);
            assert_eq!(virtual_box.rect, oracle_box.rect);
            assert_eq!(virtual_box.children[0].rect, oracle_box.children[0].rect);
            assert!(
                virtual_box.scroll_content_lines.is_none(),
                "virtual mode keeps no full array"
            );
            assert!(virtual_box.scroll_content_virtual);
        }
    }
}

fn scroll_box(frame: &rpi_tui::layout::LayoutFrame) -> &rpi_tui::layout::LayoutBox {
    get_scroll_view_box(
        frame,
        frame
            .primary_scroll_view
            .as_ref()
            .expect("primary scroll view"),
    )
    .expect("scroll box")
}

#[test]
fn height_index_rows_and_prompt_index_match_the_full_render() {
    let fx = fixture();
    let (oracle, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::None);
    let mut oracle_cache = LayoutRenderCache::new();
    let mut virtual_cache = LayoutRenderCache::new();
    let (oracle_frame, virtual_frame) = frames_at(
        &oracle,
        &virtual_,
        0,
        40,
        8,
        &mut oracle_cache,
        &mut virtual_cache,
    );
    let full = scroll_box(&oracle_frame)
        .scroll_content_lines
        .clone()
        .expect("oracle keeps the full array");
    let document = virtual_frame
        .primary_scroll_view
        .as_ref()
        .and_then(|view| with_transcript(&child_of(view), |document| document.sync(40)))
        .expect("transcript document");
    assert_eq!(document.height, full.len());
    let child = child_of(virtual_frame.primary_scroll_view.as_ref().expect("primary"));
    with_transcript(&child, |document| {
        assert_eq!(document.total_height(), full.len());
        assert_eq!(document.child_count(), 7);
        let mut row = 0usize;
        for index in 0..document.child_count() {
            assert_eq!(document.block_row(index), row);
            let height = document.height_at(index).expect("measured");
            // Index 3 is the untrusted block: no revision.
            assert_eq!(document.revision_at(index).is_some(), index != 3);
            row += height;
        }
        assert_eq!(row, full.len());
        assert_eq!(document.height_at(7), None);
        // Every row, every contiguous range and the whole corpus.
        for r in 0..full.len() {
            assert_eq!(document.line(r, 40).as_deref(), Some(full[r].as_str()));
        }
        for start in 0..full.len() {
            for len in [1usize, 3, 9] {
                let end = (start + len).min(full.len());
                assert_eq!(
                    document.rows(start..end, 40).as_ref(),
                    &full[start..end],
                    "rows {start}..{end}"
                );
            }
        }
        assert_eq!(document.materialize(40).as_ref(), full.as_ref());
        // `render_range` (child indices) equals the concatenation oracle.
        let range = document.render_range(2..5, 40);
        let start = document.block_row(2);
        let end = document.block_row(5);
        assert_eq!(range.as_ref(), &full[start..end]);
        // OSC 133 prompt index equals the row scan over the full array.
        let scanned: Vec<usize> = full
            .iter()
            .enumerate()
            .filter(|(_, line)| is_osc133_prompt_start(line))
            .map(|(row, _)| row)
            .collect();
        assert_eq!(scanned, vec![0, 40]);
        assert_eq!(document.prompt_rows(), scanned);
        // Visible block range covers the viewport window rows.
        for scroll_top in [0usize, 5, 20, full.len() - 1] {
            let range = document.visible_range(scroll_top, 8, TRANSCRIPT_OVERSCAN_ROWS);
            let first_row = document.block_row(range.start);
            let last_row = document.block_row(range.end);
            assert!(
                first_row <= scroll_top,
                "{scroll_top}: {first_row}..{last_row}"
            );
            assert!(
                last_row >= (scroll_top + 8).min(full.len()),
                "{scroll_top}: {first_row}..{last_row}"
            );
        }
    });
}

fn child_of(scroll_view: &SharedComponent) -> SharedComponent {
    with_scroll_view(scroll_view, ScrollView::child)
}

// ---- FR-B R2/R4: off-screen history is not re-rendered -------------------

#[test]
fn stable_offscreen_history_is_not_rerendered_and_appends_touch_the_tail_only() {
    let container = Arc::new(Mutex::new(Container::new()));
    let mut renders = Vec::new();
    let mut revisions = Vec::new();
    for index in 0..200 {
        let (block, block_renders, revision) = trusted(
            (0..10)
                .map(|line| format!("block {index} line {line}"))
                .collect(),
        );
        lock(&container).children.push(block);
        renders.push(block_renders);
        revisions.push(revision);
    }
    let mut fx = Fixture {
        container: Arc::clone(&container),
        trusted: renders
            .iter()
            .cloned()
            .zip(revisions.iter().cloned())
            .collect(),
        untrusted_renders: Vec::new(),
    };
    let (oracle, virtual_) = pair(&fx, ScrollbarMode::Auto, Follow::End);
    let mut oracle_cache = LayoutRenderCache::new();
    let mut virtual_cache = LayoutRenderCache::new();
    // Render counts are asserted on the virtual view alone (the oracle's
    // full render counts too).
    let first = frame(&virtual_, 30, 8, &mut virtual_cache);
    assert_eq!(
        total_renders(&fx),
        200,
        "the first frame measures every block once"
    );
    let child = child_of(first.primary_scroll_view.as_ref().expect("primary"));

    // Fifty frames of scrolling (follow-end pinned, then user scroll-back):
    // zero renders.
    for scroll_top in [0i64, 1992, 1500, 700, 100, 33] {
        with_scroll_view(&virtual_, |view| view.scroll_to(scroll_top));
        frame(&virtual_, 30, 8, &mut virtual_cache);
    }
    assert_eq!(
        total_renders(&fx),
        200,
        "scrolling never renders off-screen history"
    );
    with_transcript(&child, |document| {
        let stats = document.stats();
        assert_eq!(stats.blocks, 200);
        assert!(stats.trusted_hits >= 6 * 200);
        assert_eq!(stats.fallback_renders, 0);
        assert_eq!(
            stats.on_demand_renders, 0,
            "the working set stays retained while scrolling"
        );
    });

    // A revision bump in one block renders that block only.
    fx.trusted[100].1.store(5, Ordering::Relaxed);
    frame(&virtual_, 30, 8, &mut virtual_cache);
    assert_eq!(fx.trusted[100].0.load(Ordering::Relaxed), 2);
    assert_eq!(total_renders(&fx), 201);

    // Appending a block renders the new block only; inserting one shifts
    // the tail, which keeps its heights through the identity carry-over.
    let (block, block_renders, revision) = trusted(vec!["appended".to_string()]);
    lock(&container).children.push(block);
    fx.trusted.push((Arc::clone(&block_renders), revision));
    frame(&virtual_, 30, 8, &mut virtual_cache);
    assert_eq!(total_renders(&fx), 202);
    let (block, block_renders, revision) = trusted(vec!["inserted".to_string()]);
    lock(&container).children.insert(3, block);
    fx.trusted.push((Arc::clone(&block_renders), revision));
    frame(&virtual_, 30, 8, &mut virtual_cache);
    assert_eq!(block_renders.load(Ordering::Relaxed), 1);
    assert_eq!(total_renders(&fx), 203, "shifted blocks keep their heights");

    // A width change invalidates the height index: one re-measure, then
    // frames still agree with the oracle at the new width. (The oracle is
    // warmed up first so both views share the same follow-end history.)
    let before_width = total_renders(&fx);
    frame(&oracle, 31, 8, &mut oracle_cache);
    frame(&virtual_, 31, 8, &mut virtual_cache);
    assert_eq!(
        total_renders(&fx),
        before_width + 2 * 202,
        "oracle full render + one virtual re-measure"
    );
    for scroll_top in [0i64, 700, 1400, 2000] {
        let before = with_transcript(&child, |document| {
            let stats = document.stats();
            stats.trusted_renders + stats.on_demand_renders + stats.fallback_renders
        })
        .expect("document");
        let (oracle_frame, virtual_frame) = frames_at(
            &oracle,
            &virtual_,
            scroll_top,
            31,
            8,
            &mut oracle_cache,
            &mut virtual_cache,
        );
        assert_eq!(
            oracle_frame.lines, virtual_frame.lines,
            "width 31, scroll {scroll_top}"
        );
        let after = with_transcript(&child, |document| {
            let stats = document.stats();
            stats.trusted_renders + stats.on_demand_renders + stats.fallback_renders
        })
        .expect("document");
        assert_eq!(
            after, before,
            "no virtual render while scrolling at the new width"
        );
    }
}

// ---- FR-A R3 / FR-B identity: ABA -----------------------------------------

#[test]
fn a_replaced_block_with_the_same_revision_never_serves_the_old_lines() {
    let container = Arc::new(Mutex::new(Container::new()));
    let (old, old_renders, _) = trusted(vec!["old block".to_string()]);
    lock(&container).children.push(old);
    // Same revision (0) and the same width: only the instance id differs.
    let (new, new_renders, _) = trusted(vec!["new block".to_string()]);
    let fx = Fixture {
        container: Arc::clone(&container),
        trusted: Vec::new(),
        untrusted_renders: Vec::new(),
    };
    let (_, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::End);
    let mut cache = LayoutRenderCache::new();
    let first = frame(&virtual_, 20, 4, &mut cache);
    assert_eq!(visible(&first.lines)[0], "old block");
    assert_eq!(old_renders.load(Ordering::Relaxed), 1);

    // Replace in place. The old block is dropped here, so the allocator may
    // hand a later allocation the same address; the equal revision alone
    // must never validate the stale lines — only the instance id rules the
    // replaced block out.
    lock(&container).children[0] = new;
    let second = frame(&virtual_, 20, 4, &mut cache);
    assert_eq!(visible(&second.lines)[0], "new block");
    assert_eq!(
        new_renders.load(Ordering::Relaxed),
        1,
        "the replaced block is rendered"
    );
    assert_eq!(old_renders.load(Ordering::Relaxed), 1);
    let child = child_of(second.primary_scroll_view.as_ref().expect("primary"));
    with_transcript(&child, |document| {
        assert_eq!(document.line(0, 20).as_deref(), Some("new block"));
        let stats = document.stats();
        assert_eq!(stats.trusted_renders, 2);
        assert_eq!(stats.trusted_hits, 0);
    });
}

// ---- FR-B R4: untrusted fallback is local, per-frame and counted ---------

#[test]
fn untrusted_blocks_fall_back_per_frame_without_disabling_virtualization() {
    let fx = fixture();
    let (_, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::End);
    let mut cache = LayoutRenderCache::new();
    let child = {
        let first = frame(&virtual_, 40, 8, &mut cache);
        child_of(first.primary_scroll_view.as_ref().expect("primary"))
    };
    let trusted_renders = || {
        fx.trusted
            .iter()
            .map(|(renders, _)| renders.load(Ordering::Relaxed))
            .sum::<u64>()
    };
    assert_eq!(trusted_renders(), 4);
    assert_eq!(fx.untrusted_renders[0].load(Ordering::Relaxed), 1);
    for _ in 0..10 {
        frame(&virtual_, 40, 8, &mut cache);
    }
    assert_eq!(
        trusted_renders(),
        4,
        "trusted blocks are not rendered again"
    );
    assert_eq!(
        fx.untrusted_renders[0].load(Ordering::Relaxed),
        11,
        "untrusted blocks render every frame"
    );
    with_transcript(&child, |document| {
        let stats = document.stats();
        assert_eq!(stats.blocks, 7);
        // Root: three message blocks + the spacer; nested: `Text` + block.
        assert_eq!(stats.trusted_blocks, 6);
        assert_eq!(stats.fallback_blocks, 1);
        assert_eq!(stats.fallback_renders, 11);
        assert_eq!(stats.syncs, 11);
        // Ten further frames validate six trusted blocks each without a render.
        assert!(stats.trusted_hits >= 10 * 6);
    });
}

// ---- FR-E R2: retained lines are bounded ---------------------------------

#[test]
fn retained_lines_are_bounded_and_the_working_set_is_kept() {
    let container = Arc::new(Mutex::new(Container::new()));
    let mut renders = Vec::new();
    for index in 0..600 {
        let (block, block_renders, _) =
            trusted((0..100).map(|line| format!("b{index} l{line}")).collect());
        lock(&container).children.push(block);
        renders.push(block_renders);
    }
    let fx = Fixture {
        container,
        trusted: Vec::new(),
        untrusted_renders: Vec::new(),
    };
    let (_, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::End);
    let mut cache = LayoutRenderCache::new();
    let child = {
        let first = frame(&virtual_, 20, 10, &mut cache);
        child_of(first.primary_scroll_view.as_ref().expect("primary"))
    };
    // 600 x 100 lines were measured in the first frame; from the next frame
    // on only a bounded set beyond the working set is retained.
    frame(&virtual_, 20, 10, &mut cache);
    with_transcript(&child, |document| {
        let stats = document.stats();
        assert!(
            stats.retained_children <= MAX_RETAINED_CHILDREN + 4,
            "{} retained children",
            stats.retained_children
        );
        assert!(
            stats.retained_lines <= MAX_RETAINED_LINES + 4 * 100,
            "{} retained lines",
            stats.retained_lines
        );
        assert!(stats.evictions > 0);
    });
    // Frames at the end (the working set) render nothing new.
    let before: u64 = renders.iter().map(|r| r.load(Ordering::Relaxed)).sum();
    for _ in 0..5 {
        frame(&virtual_, 20, 10, &mut cache);
    }
    let after: u64 = renders.iter().map(|r| r.load(Ordering::Relaxed)).sum();
    assert_eq!(before, after, "the visible tail stays retained");
    // Scrolling far back re-renders only the blocks entering the window.
    with_scroll_view(&virtual_, |view| view.scroll_to(0));
    frame(&virtual_, 20, 10, &mut cache);
    let after: u64 = renders.iter().map(|r| r.load(Ordering::Relaxed)).sum();
    assert!(after - before <= 2, "{} on-demand renders", after - before);
}

// ---- FR-A: cross-frame layout cache ---------------------------------------

/// Forwards the identity protocol of a trusted leaf through a shared
/// wrapper (the application's `SharedChild` pattern).
struct IdentityForwarder {
    inner: SharedComponent,
}

impl Component for IdentityForwarder {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .render(width)
    }
    fn invalidate(&mut self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .invalidate();
    }
    fn cache_revision(&self) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cache_revision()
    }
    fn cache_instance_id(&self) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cache_instance_id()
    }
}

fn stack_root(leaf: SharedComponent) -> SharedComponent {
    // An untrusted dock keeps the cache counters about the leaf alone.
    let (dock, _) = untrusted(vec!["dock".to_string()]);
    shared_component(VStack::new(
        vec![
            StackChild::Component(leaf),
            StackChild::Component(shared_component_from_boxed(dock)),
        ],
        StackOptions::default(),
    ))
}

#[test]
fn cross_frame_cache_skips_renders_for_unchanged_trusted_components() {
    let (block, renders, revision) = trusted(vec!["hello".to_string()]);
    let inner = shared_component(TrustedBlock {
        renders: Arc::clone(&renders),
        revision: Arc::clone(&revision),
        id: next_cache_instance_id(),
        lines: vec!["hello".to_string()],
    });
    drop(block);
    let wrapper = shared_component(IdentityForwarder {
        inner: inner.clone(),
    });
    let root = stack_root(wrapper.clone());
    let mut cache = LayoutRenderCache::new();
    frame(&root, 20, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 1);
    assert_eq!(cache.stats().misses, 1);
    frame(&root, 20, 4, &mut cache);
    frame(&root, 20, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 1, "0 render() on a hit");
    assert_eq!(cache.stats().hits, 2);
    assert_eq!(cache.stats().retained_entries, 1);
    // Hits clone the Arc: the frame shares the retained lines.
    let third = frame(&root, 20, 4, &mut cache);
    assert_eq!(visible(&third.lines), vec!["hello", "dock", "", ""]);

    // A revision change renders once.
    revision.store(1, Ordering::Relaxed);
    frame(&root, 20, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 2);
    frame(&root, 20, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 2);

    // A width change is a different key.
    frame(&root, 21, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 3);
    frame(&root, 21, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 3);

    // Invalidation (theme) bumps the generation: one render, then hits.
    cache.invalidate();
    frame(&root, 21, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 4);
    frame(&root, 21, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 4);
    assert_eq!(cache.stats().invalidations, 1);
    assert_eq!(cache.stats().retained_entries, 1);

    // Removal (component leaving the tree) drops the entry.
    cache.remove_component(&wrapper);
    assert_eq!(cache.stats().retained_entries, 0);
    frame(&root, 21, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 5);
}

#[test]
fn untrusted_components_render_every_frame_and_are_never_cached() {
    let (block, renders) = untrusted(vec!["untrusted".to_string()]);
    let root = stack_root(shared_component_from_boxed(block));
    let mut cache = LayoutRenderCache::new();
    frame(&root, 20, 4, &mut cache);
    frame(&root, 20, 4, &mut cache);
    assert_eq!(renders.load(Ordering::Relaxed), 2);
    // The leaf and the untrusted dock each render once per frame.
    assert_eq!(cache.stats().untrusted_renders, 4);
    assert_eq!(cache.stats().hits, 0);
    assert_eq!(cache.stats().retained_entries, 0);
    // The default `cache_instance_id`/`as_transcript_document` are `None`.
    let text = Text::new("x", 0, 0, None);
    assert!(text.cache_instance_id().is_some());
    assert!(text.as_transcript_document().is_none());
    assert!(Container::new().as_transcript_document().is_some());
}

// ---- follow-end, append and header growth -------------------------------

#[test]
fn follow_end_appends_and_user_scroll_back_keep_the_oracle_semantics() {
    let container = Arc::new(Mutex::new(Container::new()));
    let (block, renders, revision) = trusted((0..40).map(|i| format!("row {i}")).collect());
    lock(&container).children.push(block);
    let mut fx = Fixture {
        container: Arc::clone(&container),
        trusted: vec![(renders, revision)],
        untrusted_renders: Vec::new(),
    };
    let (oracle, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::End);
    let mut oracle_cache = LayoutRenderCache::new();
    let mut virtual_cache = LayoutRenderCache::new();
    let (o, v) = frames_at(
        &oracle,
        &virtual_,
        0,
        20,
        6,
        &mut oracle_cache,
        &mut virtual_cache,
    );
    assert_eq!(o.lines, v.lines);
    assert_eq!(
        visible(&v.lines),
        (34..40).map(|i| format!("row {i}")).collect::<Vec<_>>()
    );

    // User scrolls back, then content is appended: the visible range stays.
    let (_o, v) = frames_at(
        &oracle,
        &virtual_,
        10,
        20,
        6,
        &mut oracle_cache,
        &mut virtual_cache,
    );
    let (block, renders2, revision2) = trusted((0..5).map(|i| format!("tail {i}")).collect());
    lock(&container).children.push(block);
    fx.trusted.push((Arc::clone(&renders2), revision2));
    let (o2, v2) = frames_at(
        &oracle,
        &virtual_,
        10,
        20,
        6,
        &mut oracle_cache,
        &mut virtual_cache,
    );
    assert_eq!(o2.lines, v2.lines);
    assert_eq!(
        visible(&v2.lines),
        visible(&v.lines),
        "a static scroll-back does not move"
    );
    assert_eq!(visible(&v2.lines)[0], "row 10");
    // The oracle's full render and the virtual view each render the new block once.
    assert_eq!(renders2.load(Ordering::Relaxed), 2);

    // Back to the end: the follow pins to the new bottom.
    with_scroll_view(&oracle, |view| view.scroll_to_end());
    with_scroll_view(&virtual_, |view| view.scroll_to_end());
    let o3 = frame(&oracle, 20, 6, &mut oracle_cache);
    let v3 = frame(&virtual_, 20, 6, &mut virtual_cache);
    assert_eq!(o3.lines, v3.lines);
    assert_eq!(visible(&v3.lines)[0], "row 39");
    // The first frame (follow-end) showed the bottom of the single block.
    assert_eq!(visible(&v2.lines)[0], "row 10");
}

#[test]
fn instance_ids_are_process_unique() {
    let a = next_cache_instance_id();
    let b = next_cache_instance_id();
    assert_ne!(a, b);
    let (one, _, _) = trusted(Vec::new());
    let (two, _, _) = trusted(Vec::new());
    drop((one, two));
}

// ---- resize between frames (shared state across widths) -----------------

#[test]
fn resize_frames_match_the_oracle_when_the_width_and_height_change_between_frames() {
    let fx = fixture();
    for (scrollbar, follow) in [
        (ScrollbarMode::Auto, Follow::End),
        (ScrollbarMode::Hidden, Follow::None),
    ] {
        let (oracle, virtual_) = pair(&fx, scrollbar, follow);
        let mut oracle_cache = LayoutRenderCache::new();
        let mut virtual_cache = LayoutRenderCache::new();
        for (width, height, scroll_top) in [
            (20usize, 4usize, None),
            (40, 12, None),
            (40, 12, Some(7)),
            (12, 3, None),
            (12, 3, None),
            (30, 9, Some(0)),
            (30, 9, Some(100)),
        ] {
            if let Some(scroll_top) = scroll_top {
                with_scroll_view(&oracle, |view| view.scroll_to(scroll_top));
                with_scroll_view(&virtual_, |view| view.scroll_to(scroll_top));
            }
            let oracle_frame = frame(&oracle, width, height, &mut oracle_cache);
            let virtual_frame = frame(&virtual_, width, height, &mut virtual_cache);
            assert_eq!(
                oracle_frame.lines, virtual_frame.lines,
                "{width}x{height} scroll {scroll_top:?} ({scrollbar:?}/{follow:?})"
            );
            let o = scroll_box(&oracle_frame);
            let v = scroll_box(&virtual_frame);
            assert_eq!(o.rect, v.rect, "scroll rect {width}x{height}");
            assert_eq!(
                o.children[0].rect, v.children[0].rect,
                "content rect {width}x{height}"
            );
        }
    }
}

// ---- round-1 review findings ---------------------------------------------

/// A block with an instance id but no trusted revision (a Markdown behind an
/// opaque extension transformer, an assistant message with one untrusted
/// child): untrusted, and still rendered exactly once per frame.
struct IdentityOnlyBlock {
    renders: Arc<AtomicU64>,
    id: u64,
    lines: Vec<String>,
}

impl Component for IdentityOnlyBlock {
    fn render(&self, _width: usize) -> Vec<String> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        self.lines.clone()
    }
    fn cache_instance_id(&self) -> Option<u64> {
        Some(self.id)
    }
}

#[test]
fn identity_without_a_revision_is_an_untrusted_block_rendered_once_per_frame() {
    let container = Arc::new(Mutex::new(Container::new()));
    let renders = Arc::new(AtomicU64::new(0));
    lock(&container).children.push(Box::new(IdentityOnlyBlock {
        renders: Arc::clone(&renders),
        id: next_cache_instance_id(),
        lines: vec!["status line".to_string(), "second row".to_string()],
    }));
    let (block, _, _) = trusted(vec!["stable answer".to_string()]);
    lock(&container).children.push(block);
    let fx = Fixture {
        container: Arc::clone(&container),
        trusted: Vec::new(),
        untrusted_renders: Vec::new(),
    };
    let (oracle, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::End);
    let mut oracle_cache = LayoutRenderCache::new();
    let mut virtual_cache = LayoutRenderCache::new();
    let mut child = None;
    for _ in 0..11 {
        let (oracle_frame, virtual_frame) = frames_at(
            &oracle,
            &virtual_,
            0,
            20,
            4,
            &mut oracle_cache,
            &mut virtual_cache,
        );
        assert_eq!(oracle_frame.lines, virtual_frame.lines);
        child = Some(child_of(
            virtual_frame.primary_scroll_view.as_ref().expect("primary"),
        ));
    }
    assert_eq!(
        renders.load(Ordering::Relaxed),
        22,
        "one render per frame in each view"
    );
    with_transcript(&child.expect("child"), |document| {
        let stats = document.stats();
        assert_eq!(stats.fallback_blocks, 1);
        assert_eq!(stats.fallback_renders, 11, "the sync render only");
        assert_eq!(
            stats.on_demand_renders, 0,
            "the window reuses the frame's lines"
        );
    });
}

#[test]
fn search_corpus_survives_a_double_sync_frame_and_is_released_after_two_idle_syncs() {
    // Trusted blocks only: an untrusted block changes the content stamp every
    // frame, which would rebuild the corpus regardless of the release rule.
    let container = Arc::new(Mutex::new(Container::new()));
    let (block, _, _) = trusted((0..40).map(|index| format!("row {index}")).collect());
    lock(&container).children.push(block);
    let (_, virtual_) = pair(
        &Fixture {
            container,
            trusted: Vec::new(),
            untrusted_renders: Vec::new(),
        },
        ScrollbarMode::Hidden,
        Follow::End,
    );
    let mut cache = LayoutRenderCache::new();
    let child = child_of(
        frame(&virtual_, 20, 4, &mut cache)
            .primary_scroll_view
            .as_ref()
            .expect("primary"),
    );
    with_transcript(&child, |document| {
        assert_eq!(document.materialize(20).len(), 40);
        assert_eq!(document.stats().corpus_rebuilds, 1);
        // A search reveal lays out twice in one real frame: the corpus must
        // survive a sync that does not consume it.
        document.sync(20);
        assert_eq!(document.materialize(20).len(), 40);
        assert_eq!(
            document.stats().corpus_rebuilds,
            1,
            "no rebuild after one idle sync"
        );
        // Search closed: the copy survives two idle syncs and is released at
        // the third; the next consumer rebuilds it exactly once.
        document.sync(20);
        document.sync(20);
        document.sync(20);
        assert_eq!(document.materialize(20).len(), 40);
        assert_eq!(document.stats().corpus_rebuilds, 2);
    });
}

/// An identity-only block whose content the test can change between frames.
struct MutableIdentityOnlyBlock {
    renders: Arc<AtomicU64>,
    id: u64,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Component for MutableIdentityOnlyBlock {
    fn render(&self, _width: usize) -> Vec<String> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        lock(&self.lines).clone()
    }
    fn cache_instance_id(&self) -> Option<u64> {
        Some(self.id)
    }
}

#[test]
fn search_corpus_follows_the_latest_render_of_an_identity_only_block() {
    // Round-2 review finding: the corpus segment of an untrusted block that
    // exposes an instance id must not be reused across frames, or an open
    // search keeps matching the block's first render.
    let container = Arc::new(Mutex::new(Container::new()));
    let renders = Arc::new(AtomicU64::new(0));
    let lines = Arc::new(Mutex::new(vec!["alpha one".to_string()]));
    lock(&container)
        .children
        .push(Box::new(MutableIdentityOnlyBlock {
            renders: Arc::clone(&renders),
            id: next_cache_instance_id(),
            lines: Arc::clone(&lines),
        }));
    let fx = Fixture {
        container: Arc::clone(&container),
        trusted: Vec::new(),
        untrusted_renders: Vec::new(),
    };
    let (_, virtual_) = pair(&fx, ScrollbarMode::Hidden, Follow::End);
    let mut cache = LayoutRenderCache::new();
    let child = child_of(
        frame(&virtual_, 20, 4, &mut cache)
            .primary_scroll_view
            .as_ref()
            .expect("primary"),
    );
    let corpus = |width: usize| {
        with_transcript(&child, |document| {
            document
                .materialize(width)
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        })
        .expect("document")
    };
    assert_eq!(corpus(20), vec!["alpha one".to_string()]);
    // The block changes: the next sync re-renders it (untrusted), and both
    // the visible rows and the search corpus must show the new content.
    *lock(&lines) = vec!["beta two".to_string()];
    let next = frame(&virtual_, 20, 4, &mut cache);
    assert!(visible(&next.lines)[0].starts_with("beta two"));
    assert_eq!(corpus(20), vec!["beta two".to_string()]);
    assert!(corpus(20).iter().all(|line| !line.contains("alpha")));
    assert!(renders.load(Ordering::Relaxed) >= 2);
}
