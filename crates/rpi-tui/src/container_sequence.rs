//! Per-container block sequence (V19-06): the height index and bounded
//! retained-lines set behind `Container`'s [`TranscriptDocument`] impl.
//!
//! The state lives in a `RefCell` inside the container and is only touched by
//! the transcript contract — `Container::render` keeps its frozen full-render
//! behavior and never reads it. See `transcript.rs` for the identity protocol
//! and the consumer contract.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::transcript::{
    MAX_RETAINED_CHILDREN, MAX_RETAINED_LINES, TranscriptStats, TranscriptSync, clamp_window,
    prompt_offsets,
};
use crate::tui::Component;

/// One direct child of the container as seen by the height index.
pub(crate) enum Slot {
    /// A block that renders itself (`render`), cached by identity/revision
    /// when trusted.
    Opaque(OpaqueSlot),
    /// A nested transcript document (a wrapper of another container).
    Nested(NestedSlot),
}

pub(crate) struct OpaqueSlot {
    identity: Option<u64>,
    revision: Option<u64>,
    /// Whether `height` is known at the state's current width.
    measured: bool,
    height: usize,
    /// Row offsets of the OSC 133 prompt-start lines inside the block.
    prompt_offsets: Arc<[u32]>,
    /// Lines of an untrusted block as rendered by the latest sync (valid for
    /// that frame only). Trusted blocks keep their lines in the retained set.
    lines: Option<Arc<[String]>>,
}

impl OpaqueSlot {
    fn unknown(identity: Option<u64>) -> Self {
        OpaqueSlot {
            identity,
            revision: None,
            measured: false,
            height: 0,
            prompt_offsets: Arc::from(Vec::new()),
            lines: None,
        }
    }
}

pub(crate) struct NestedSlot {
    stamp: u64,
    height: usize,
    flat_count: usize,
    prompt_rows: Arc<[usize]>,
}

impl NestedSlot {
    fn unknown() -> Self {
        NestedSlot {
            stamp: 0,
            height: 0,
            flat_count: 0,
            prompt_rows: Arc::from(Vec::new()),
        }
    }
}

struct RetainedLines {
    revision: u64,
    lines: Arc<[String]>,
    /// Last touch (any access) on the state clock.
    touch: u64,
    /// Last touch through a row-range access (the working set), if any.
    window_touch: Option<u64>,
}

/// Bounded set of trusted block lines kept across frames.
#[derive(Default)]
struct RetainedSet {
    entries: HashMap<u64, RetainedLines>,
    lines: usize,
}

impl RetainedSet {
    fn insert(
        &mut self,
        identity: u64,
        revision: u64,
        lines: Arc<[String]>,
        touch: u64,
        window: bool,
    ) {
        let previous_window_touch = self
            .entries
            .get(&identity)
            .and_then(|entry| entry.window_touch);
        if let Some(previous) = self.entries.insert(
            identity,
            RetainedLines {
                revision,
                lines,
                touch,
                // A re-measured working-set block keeps its window touch:
                // eviction must not pick it before the next row access.
                window_touch: if window {
                    Some(touch)
                } else {
                    previous_window_touch
                },
            },
        ) {
            self.lines = self.lines.saturating_sub(previous.lines.len());
        }
        if let Some(current) = self.entries.get(&identity) {
            self.lines += current.lines.len();
        }
    }

    fn touch_window(&mut self, identity: u64, revision: u64, touch: u64) -> Option<Arc<[String]>> {
        let entry = self.entries.get_mut(&identity)?;
        if entry.revision != revision {
            return None;
        }
        entry.touch = touch;
        entry.window_touch = Some(touch);
        Some(entry.lines.clone())
    }

    /// Evicts least-recently-used entries until both capacities hold. Entries
    /// window-touched since `protect_clock` (the previous working set) are
    /// never evicted; the working set itself may therefore exceed the caps,
    /// which only bound what is retained beyond it.
    fn evict(&mut self, protect_clock: u64, stats: &mut TranscriptStats) {
        if self.entries.len() <= MAX_RETAINED_CHILDREN && self.lines <= MAX_RETAINED_LINES {
            return;
        }
        let mut candidates: Vec<(u64, u64, u64, usize)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.window_touch.is_none_or(|touch| touch < protect_clock))
            .map(|(identity, entry)| {
                (
                    entry.window_touch.unwrap_or(0),
                    entry.touch,
                    *identity,
                    entry.lines.len(),
                )
            })
            .collect();
        candidates.sort_unstable();
        for (_, _, identity, len) in candidates {
            if self.entries.len() <= MAX_RETAINED_CHILDREN && self.lines <= MAX_RETAINED_LINES {
                break;
            }
            if self.entries.remove(&identity).is_some() {
                self.lines = self.lines.saturating_sub(len);
                stats.evictions += 1;
            }
        }
    }
}

/// Identity of one corpus segment: a segment is reused while its key is
/// unchanged, so a rebuild only re-fetches the children that changed.
enum SegmentKey {
    Opaque {
        identity: Option<u64>,
        revision: Option<u64>,
        /// Untrusted lines are re-rendered by every sync, so their segments
        /// are keyed by the frame of the render (0 for trusted blocks).
        frame: u64,
    },
    Nested {
        stamp: u64,
    },
}

struct Segment {
    key: SegmentKey,
    lines: Arc<[String]>,
}

/// The lazily built full-text corpus of a container (search).
struct Corpus {
    width: usize,
    stamp: u64,
    lines: Arc<[String]>,
    segments: Vec<Segment>,
    /// Frame in which the corpus was last served.
    last_used_frame: u64,
}

/// Height index of one container's children.
#[derive(Default)]
pub(crate) struct SequenceState {
    width: usize,
    synced: bool,
    slots: Vec<Slot>,
    /// `starts[i]` is the first content row of direct child `i`;
    /// `starts[len]` is the total height.
    starts: Vec<usize>,
    total_height: usize,
    flat_count: usize,
    stamp: u64,
    frame: u64,
    /// Monotonic access clock for the retained set.
    clock: u64,
    /// Clock value when the previous frame's sync started.
    frame_start_clock: u64,
    retained: RetainedSet,
    corpus: Option<Corpus>,
    stats: TranscriptStats,
}

impl SequenceState {
    /// Drops every cached line and height (theme invalidation).
    pub(crate) fn reset(&mut self) {
        self.slots.clear();
        self.starts.clear();
        self.total_height = 0;
        self.flat_count = 0;
        self.synced = false;
        self.retained = RetainedSet::default();
        self.corpus = None;
        self.stamp = self.stamp.wrapping_add(1);
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// The direct child whose row span contains `row`. Zero-height children
    /// are skipped towards the next non-empty one, like the concatenation
    /// they do not contribute rows to.
    fn child_at_row(&self, row: usize) -> usize {
        let index = self.starts.partition_point(|start| *start <= row);
        index
            .saturating_sub(1)
            .min(self.slots.len().saturating_sub(1))
    }

    fn flat_base(&self, child: usize) -> usize {
        self.slots[..child]
            .iter()
            .map(|slot| match slot {
                Slot::Opaque(_) => 1,
                Slot::Nested(nested) => nested.flat_count,
            })
            .sum()
    }

    /// Whether the slot vector still describes `children` one-to-one. The
    /// application mounts the chat container behind an `Arc<Mutex<..>>` and
    /// restructures it from the session thread (a `/clear`, a theme rebuild,
    /// an append) while the driver thread sits between a frame's `sync` and
    /// a later read (the layout's `rows`, the residual image crop, a
    /// selection copy) — so a read can meet a `children` vector that no
    /// longer matches `slots`/`starts`. The `&self` readers then answer
    /// "unmeasured" until the next sync; the row readers re-measure first
    /// ([`Self::resync_if_restructured`]).
    fn aligned(&self, children: &[Box<dyn Component>]) -> bool {
        self.slots.len() == children.len()
    }

    /// Brings the height index back in step with `children` after a
    /// restructure that landed since the last sync, so a row reader never
    /// indexes `children` with a position from the stale slot vector. This
    /// is the next frame's `sync` moved ahead of the read: trusted blocks
    /// keep their heights through the identity carry-over, so only the
    /// blocks that actually changed render.
    fn resync_if_restructured(&mut self, children: &[Box<dyn Component>], width: usize) {
        if !self.aligned(children) {
            self.sync(children, width);
        }
    }

    /// Rebuilds the slot vector after a structural change, carrying trusted
    /// opaque slots over by identity (a shifted block keeps its height and
    /// retained lines; a replaced block starts unknown).
    fn realign(&mut self, children: &[Box<dyn Component>]) {
        let mut previous: HashMap<u64, OpaqueSlot> = HashMap::with_capacity(self.slots.len());
        for slot in self.slots.drain(..) {
            if let Slot::Opaque(opaque) = slot
                && let Some(identity) = opaque.identity
            {
                previous.insert(identity, opaque);
            }
        }
        self.slots = children
            .iter()
            .map(|child| {
                if child.as_transcript_document().is_some() {
                    Slot::Nested(NestedSlot::unknown())
                } else {
                    let identity = child.cache_instance_id();
                    match identity.and_then(|identity| previous.remove(&identity)) {
                        Some(opaque) => Slot::Opaque(opaque),
                        None => Slot::Opaque(OpaqueSlot::unknown(identity)),
                    }
                }
            })
            .collect();
        self.corpus = None;
    }

    fn record_opaque(
        &mut self,
        index: usize,
        identity: Option<u64>,
        revision: Option<u64>,
        lines: Arc<[String]>,
        window: bool,
    ) {
        let Slot::Opaque(slot) = &mut self.slots[index] else {
            return;
        };
        slot.identity = identity;
        slot.revision = revision;
        slot.measured = true;
        slot.height = lines.len();
        slot.prompt_offsets = prompt_offsets(&lines).into();
        slot.lines = None;
        match (identity, revision) {
            (Some(identity), Some(revision)) => {
                let touch = self.tick();
                self.retained
                    .insert(identity, revision, lines, touch, window);
            }
            _ => slot.lines = Some(lines),
        }
    }

    /// Measures every block whose identity or revision changed at `width`
    /// (and every untrusted block), then refreshes the row index.
    pub(crate) fn sync(&mut self, children: &[Box<dyn Component>], width: usize) -> TranscriptSync {
        self.frame = self.frame.wrapping_add(1);
        if !self.synced || self.width != width {
            self.reset();
            self.width = width;
            self.synced = true;
        }
        self.stats.syncs += 1;
        self.retained.evict(self.frame_start_clock, &mut self.stats);
        self.frame_start_clock = self.clock;
        if self
            .corpus
            .as_ref()
            .is_some_and(|corpus| corpus.last_used_frame.wrapping_add(2) < self.frame)
        {
            // No consumer asked for the corpus in the previous two syncs
            // (search closed): release the full-text copy. Two syncs, not
            // one: a search reveal renders two layouts in the same frame,
            // and the corpus must survive that second sync.
            self.corpus = None;
        }
        if self.slots.len() != children.len() {
            self.realign(children);
        }

        let mut starts = Vec::with_capacity(children.len() + 1);
        let mut total = 0usize;
        let mut flat_count = 0usize;
        let mut changed = false;
        let (mut trusted_blocks, mut fallback_blocks) = (0usize, 0usize);
        for (index, child) in children.iter().enumerate() {
            starts.push(total);
            if let Some(document) = child.as_transcript_document() {
                let nested = document.sync(width);
                let nested_count = document.child_count();
                let prompt_rows: Arc<[usize]> = document.prompt_rows().into();
                if !matches!(self.slots[index], Slot::Nested(_)) {
                    self.slots[index] = Slot::Nested(NestedSlot::unknown());
                }
                let Slot::Nested(slot) = &mut self.slots[index] else {
                    continue;
                };
                if slot.stamp != nested.stamp || slot.height != nested.height {
                    changed = true;
                }
                slot.stamp = nested.stamp;
                slot.height = nested.height;
                slot.flat_count = nested_count;
                slot.prompt_rows = prompt_rows;
                total += nested.height;
                flat_count += nested_count;
            } else {
                let identity = child.cache_instance_id();
                let revision = child.cache_revision();
                let trusted = identity.is_some() && revision.is_some();
                if trusted {
                    trusted_blocks += 1;
                } else {
                    fallback_blocks += 1;
                }
                if !matches!(self.slots[index], Slot::Opaque(_)) {
                    self.slots[index] = Slot::Opaque(OpaqueSlot::unknown(identity));
                }
                let unchanged = trusted
                    && matches!(&self.slots[index], Slot::Opaque(slot)
                        if slot.identity == identity && slot.revision == revision && slot.measured);
                if unchanged {
                    self.stats.trusted_hits += 1;
                } else {
                    // A shifted block (structural change) may still have its
                    // lines retained: reuse them instead of rendering.
                    let retained = match (identity, revision) {
                        (Some(identity), Some(revision)) => self
                            .retained
                            .entries
                            .get(&identity)
                            .filter(|entry| entry.revision == revision)
                            .map(|entry| entry.lines.clone()),
                        _ => None,
                    };
                    match retained {
                        Some(lines) => {
                            self.stats.trusted_hits += 1;
                            self.record_opaque(index, identity, revision, lines, false);
                        }
                        None => {
                            let lines: Arc<[String]> = child.render(width).into();
                            if trusted {
                                self.stats.trusted_renders += 1;
                            } else {
                                self.stats.fallback_renders += 1;
                            }
                            self.record_opaque(index, identity, revision, lines, false);
                        }
                    }
                    changed = true;
                }
                let Slot::Opaque(slot) = &self.slots[index] else {
                    continue;
                };
                total += slot.height;
                flat_count += 1;
            }
        }
        starts.push(total);
        self.starts = starts;
        self.total_height = total;
        self.flat_count = flat_count;
        if changed {
            self.stamp = self.stamp.wrapping_add(1);
        }
        self.stats.blocks = flat_count;
        self.stats.trusted_blocks = trusted_blocks;
        self.stats.fallback_blocks = fallback_blocks;
        TranscriptSync {
            stamp: self.stamp,
            height: total,
        }
    }

    pub(crate) fn total_height(&self) -> usize {
        self.total_height
    }

    /// Height of each direct child at the synced width (the container's
    /// `mouseLayout` heights, as `render` would record them).
    pub(crate) fn child_heights(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .map(|(child, _)| self.starts[child + 1] - self.starts[child])
            .collect()
    }

    pub(crate) fn child_count(&self) -> usize {
        self.flat_count
    }

    /// Resolves a flattened block index to `(direct child, local index)`.
    fn resolve(&self, index: usize) -> Option<(usize, usize)> {
        let mut remaining = index;
        for (child, slot) in self.slots.iter().enumerate() {
            let count = match slot {
                Slot::Opaque(_) => 1,
                Slot::Nested(nested) => nested.flat_count,
            };
            if remaining < count {
                return Some((child, remaining));
            }
            remaining -= count;
        }
        None
    }

    pub(crate) fn revision_at(&self, children: &[Box<dyn Component>], index: usize) -> Option<u64> {
        if !self.aligned(children) {
            return None;
        }
        let (child, local) = self.resolve(index)?;
        match &self.slots[child] {
            Slot::Opaque(slot) => slot.revision,
            Slot::Nested(nested) => {
                let document = children[child].as_transcript_document()?;
                document.revision_at(local)?;
                Some(nested.stamp)
            }
        }
    }

    pub(crate) fn height_at(&self, children: &[Box<dyn Component>], index: usize) -> Option<usize> {
        if !self.aligned(children) {
            return None;
        }
        let (child, local) = self.resolve(index)?;
        match &self.slots[child] {
            Slot::Opaque(slot) => slot.measured.then_some(slot.height),
            Slot::Nested(_) => children[child]
                .as_transcript_document()
                .and_then(|document| document.height_at(local)),
        }
    }

    pub(crate) fn block_row(&self, children: &[Box<dyn Component>], index: usize) -> usize {
        // An unmeasured read resolves to the content end (the out-of-range
        // convention): a consumer that turns block rows into a `rows` window
        // asks for no rows rather than rows of a displaced block.
        if !self.aligned(children) {
            return self.total_height;
        }
        match self.resolve(index) {
            Some((child, local)) => {
                self.starts[child]
                    + match &self.slots[child] {
                        Slot::Opaque(_) => 0,
                        Slot::Nested(_) => children[child]
                            .as_transcript_document()
                            .map_or(0, |document| document.block_row(local)),
                    }
            }
            None => self.total_height,
        }
    }

    pub(crate) fn visible_range(
        &self,
        children: &[Box<dyn Component>],
        scroll_top: usize,
        viewport: usize,
        overscan: usize,
    ) -> Range<usize> {
        let window = clamp_window(scroll_top, viewport, overscan, self.total_height);
        if window.is_empty() || !self.aligned(children) {
            return 0..0;
        }
        let first_child = self.child_at_row(window.start);
        let last_child = self.child_at_row(window.end - 1);
        let local_range = |child: usize, from: usize, to: usize| -> Range<usize> {
            let start = self.starts[child];
            match &self.slots[child] {
                Slot::Opaque(_) => 0..1,
                Slot::Nested(_) => children[child]
                    .as_transcript_document()
                    .map(|document| document.visible_range(from - start, to - from, 0))
                    .unwrap_or(0..0),
            }
        };
        let first = self.flat_base(first_child)
            + local_range(first_child, window.start, self.starts[first_child + 1]).start;
        let last_local = local_range(last_child, self.starts[last_child], window.end);
        let last = self.flat_base(last_child) + last_local.end.saturating_sub(1);
        first..(last + 1).max(first)
    }

    pub(crate) fn prompt_rows(&self, children: &[Box<dyn Component>]) -> Vec<usize> {
        let mut rows = Vec::new();
        if !self.aligned(children) {
            return rows;
        }
        for (child, slot) in self.slots.iter().enumerate() {
            let start = self.starts[child];
            match slot {
                Slot::Opaque(opaque) => rows.extend(
                    opaque
                        .prompt_offsets
                        .iter()
                        .map(|offset| start + *offset as usize),
                ),
                Slot::Nested(_) => {
                    if let Some(document) = children[child].as_transcript_document() {
                        rows.extend(document.prompt_rows().into_iter().map(|row| start + row));
                    }
                }
            }
        }
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Lines of an opaque block: the frame's sync result for untrusted
    /// blocks, the retained set for trusted ones, else an on-demand render.
    /// The height index is deliberately left untouched: callers read rows of
    /// the last synced layout and the next sync re-measures.
    fn opaque_lines(&mut self, index: usize, child: &dyn Component, width: usize) -> Arc<[String]> {
        let identity = child.cache_instance_id();
        let revision = child.cache_revision();
        if let (Some(identity), Some(revision)) = (identity, revision) {
            let slot_matches = matches!(&self.slots[index],
                Slot::Opaque(slot) if slot.identity == Some(identity) && slot.revision == Some(revision));
            if slot_matches {
                let touch = self.tick();
                if let Some(lines) = self.retained.touch_window(identity, revision, touch) {
                    return lines;
                }
            }
        } else if let Slot::Opaque(slot) = &self.slots[index]
            && slot.identity == identity
            && slot.revision == revision
            && let Some(lines) = &slot.lines
        {
            // Any block without the full identity protocol (no revision, or
            // no id and no revision) is untrusted; its frame-local lines
            // come from the latest sync, whatever its instance id says.
            return lines.clone();
        }
        self.stats.on_demand_renders += 1;
        let lines: Arc<[String]> = child.render(width).into();
        match (identity, revision) {
            (Some(identity), Some(revision)) => {
                let touch = self.tick();
                self.retained
                    .insert(identity, revision, lines.clone(), touch, true);
                if let Slot::Opaque(slot) = &mut self.slots[index] {
                    slot.lines = None;
                }
            }
            _ => {
                if let Slot::Opaque(slot) = &mut self.slots[index] {
                    slot.lines = Some(lines.clone());
                }
            }
        }
        lines
    }

    pub(crate) fn rows(
        &mut self,
        children: &[Box<dyn Component>],
        rows: Range<usize>,
        width: usize,
    ) -> Arc<[String]> {
        self.resync_if_restructured(children, width);
        let start = rows.start.min(self.total_height);
        let end = rows.end.min(self.total_height).max(start);
        if start >= end {
            return Arc::from(Vec::new());
        }
        let mut out: Vec<String> = Vec::with_capacity(end - start);
        let mut cursor = start;
        while cursor < end {
            let child = self.child_at_row(cursor);
            let child_start = self.starts[child];
            let take_end = end.min(self.starts[child + 1]);
            if matches!(self.slots[child], Slot::Nested(_)) {
                if let Some(document) = children[child].as_transcript_document() {
                    let local = document.rows(cursor - child_start..take_end - child_start, width);
                    out.extend(local.iter().cloned());
                }
            } else {
                let lines = self.opaque_lines(child, &*children[child], width);
                // A block re-rendered on demand may differ in height from
                // the synced index (content changed since the last frame);
                // never read past what it returned.
                let low = cursor - child_start;
                let high = (take_end - child_start).min(lines.len());
                if low < high {
                    out.extend(lines[low..high].iter().cloned());
                }
            }
            cursor = take_end;
        }
        out.into()
    }

    /// Full lines of flattened blocks `blocks`: the block rows resolve
    /// against the current children (a restructure since the last sync
    /// re-measures first), then the row range materializes like [`Self::rows`].
    pub(crate) fn render_range(
        &mut self,
        children: &[Box<dyn Component>],
        blocks: Range<usize>,
        width: usize,
    ) -> Arc<[String]> {
        self.resync_if_restructured(children, width);
        let rows = self.block_row(children, blocks.start)..self.block_row(children, blocks.end);
        self.rows(children, rows, width)
    }

    fn segment_key(&self, index: usize) -> SegmentKey {
        match &self.slots[index] {
            Slot::Opaque(slot) => SegmentKey::Opaque {
                identity: slot.identity,
                revision: slot.revision,
                // Only a fully trusted block (id and revision) has stable
                // lines between syncs; anything without the revision half
                // is re-rendered every frame and must refresh its segment.
                frame: if slot.identity.is_some() && slot.revision.is_some() {
                    0
                } else {
                    self.frame
                },
            },
            Slot::Nested(nested) => SegmentKey::Nested {
                stamp: nested.stamp,
            },
        }
    }

    /// Builds (or reuses) the full-text corpus at `width`.
    pub(crate) fn materialize(
        &mut self,
        children: &[Box<dyn Component>],
        width: usize,
    ) -> Arc<[String]> {
        self.resync_if_restructured(children, width);
        if let Some(corpus) = &mut self.corpus
            && corpus.width == width
            && corpus.stamp == self.stamp
            && corpus.segments.len() == children.len()
        {
            corpus.last_used_frame = self.frame;
            return corpus.lines.clone();
        }
        let reusable = self.corpus.take().filter(|corpus| corpus.width == width);
        let mut segments: Vec<Segment> = Vec::with_capacity(children.len());
        let mut total = 0usize;
        for (index, child) in children.iter().enumerate() {
            let key = self.segment_key(index);
            let reused = reusable.as_ref().and_then(|corpus| {
                corpus
                    .segments
                    .get(index)
                    .filter(|segment| same_key(&segment.key, &key))
                    .map(|segment| segment.lines.clone())
            });
            let lines = match reused {
                Some(lines) => lines,
                None => match &self.slots[index] {
                    Slot::Nested(_) => child
                        .as_transcript_document()
                        .map(|document| document.materialize(width))
                        .unwrap_or_default(),
                    Slot::Opaque(_) => self.opaque_lines(index, &**child, width),
                },
            };
            total += lines.len();
            segments.push(Segment { key, lines });
        }
        let mut out = Vec::with_capacity(total);
        for segment in &segments {
            out.extend(segment.lines.iter().cloned());
        }
        let lines: Arc<[String]> = out.into();
        self.stats.corpus_rebuilds += 1;
        self.corpus = Some(Corpus {
            width,
            stamp: self.stamp,
            lines: lines.clone(),
            segments,
            last_used_frame: self.frame,
        });
        lines
    }

    pub(crate) fn stats(&self) -> TranscriptStats {
        let mut stats = self.stats;
        stats.retained_children = self.retained.entries.len();
        stats.retained_lines = self.retained.lines;
        stats
    }

    /// Whether direct child `child` is a nested document.
    pub(crate) fn is_nested(&self, child: usize) -> bool {
        matches!(self.slots.get(child), Some(Slot::Nested(_)))
    }
}

fn same_key(a: &SegmentKey, b: &SegmentKey) -> bool {
    match (a, b) {
        (
            SegmentKey::Opaque {
                identity: a_identity,
                revision: a_revision,
                frame: a_frame,
            },
            SegmentKey::Opaque {
                identity: b_identity,
                revision: b_revision,
                frame: b_frame,
            },
        ) => a_identity == b_identity && a_revision == b_revision && a_frame == b_frame,
        (SegmentKey::Nested { stamp: a }, SegmentKey::Nested { stamp: b }) => a == b,
        _ => false,
    }
}
