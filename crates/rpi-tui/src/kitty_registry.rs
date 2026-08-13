//! Kitty image upload cache and placement-only screen preparation for the
//! fullscreen renderer (T31).
//!
//! Port of `packages/tui/src/tui-alt-screen.ts` @ pi 0.84.1+ (4181f66): the
//! `uploadedKittyImages` cache (tui-alt-screen.ts:64-68, 144, 221, 262) and
//! `prepareKittyScreen` (tui-alt-screen.ts:294-342), extracted into a
//! standalone submodule so the alternate-screen renderer can reuse them
//! directly. `getKittyImagePlacement` / `deleteKittyImage` live in
//! terminal_image.rs (terminal-image.ts:211-217, 348-380).
//!
//! Intentional differences:
//! - Upstream keeps the cache as a per-instance field of `TuiAltScreen`; here
//!   it is a process-global registry (like `kittyImageMetadata` in
//!   terminal_image.rs) with `clear_kitty_image_cache` replacing upstream's
//!   `uploadedKittyImages.clear()` calls on alternate-screen start/stop.
//! - `estimated_decoded_bytes` stays `f64` to mirror upstream's JS `number`
//!   arithmetic (`widthPx * heightPx * 4`); the quota constants are exact in
//!   `f64`, so comparisons behave identically.
//! - The `delete` + `set` LRU touch is modeled with a `HashMap` plus an
//!   insertion-order `VecDeque`, like `KittyImageMetadataRegistry` in
//!   terminal_image.rs; the cache is tiny (bounded by the 16-image quota),
//!   so `retain`-based removal is not a hot path.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use crate::terminal_image::{delete_kitty_image, get_kitty_image_placement};

/// `CachedKittyImage` (tui-alt-screen.ts:64-68 @ 4181f66).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CachedKittyImage {
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: f64,
}

/// `MAX_CACHED_OFFSCREEN_KITTY_IMAGES` (tui-alt-screen.ts:58 @ 4181f66).
const MAX_CACHED_OFFSCREEN_KITTY_IMAGES: usize = 16;

/// `MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES` (tui-alt-screen.ts:59).
const MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES: usize = 32 * 1024 * 1024;

/// `MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES` (tui-alt-screen.ts:60).
const MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES: f64 = 64.0 * 1024.0 * 1024.0;

/// `uploadedKittyImages` (tui-alt-screen.ts:144 @ 4181f66): an
/// insertion-ordered map; the LRU position of an id is refreshed by
/// `delete` + `set` on every visible placement, exactly like upstream's JS
/// `Map`.
struct KittyImageCache {
    map: HashMap<u32, CachedKittyImage>,
    order: VecDeque<u32>,
}

impl KittyImageCache {
    fn new() -> Self {
        KittyImageCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, image_id: u32) -> Option<CachedKittyImage> {
        self.map.get(&image_id).copied()
    }

    /// `uploadedKittyImages.delete(id)` + `.set(id, entry)`
    /// (tui-alt-screen.ts:307-308): the LRU touch.
    fn set(&mut self, image_id: u32, image: CachedKittyImage) {
        if self.map.remove(&image_id).is_some() {
            self.order.retain(|&id| id != image_id);
        }
        self.map.insert(image_id, image);
        self.order.push_back(image_id);
    }

    fn remove(&mut self, image_id: u32) {
        if self.map.remove(&image_id).is_some() {
            self.order.retain(|&id| id != image_id);
        }
    }

    /// Snapshot in insertion order (oldest first), like iterating a JS `Map`.
    fn entries_in_order(&self) -> Vec<(u32, CachedKittyImage)> {
        self.order
            .iter()
            .filter_map(|&id| self.map.get(&id).copied().map(|image| (id, image)))
            .collect()
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

static KITTY_IMAGE_CACHE: Mutex<Option<KittyImageCache>> = Mutex::new(None);

fn lock_kitty_image_cache() -> MutexGuard<'static, Option<KittyImageCache>> {
    KITTY_IMAGE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `uploadedKittyImages.clear()` (tui-alt-screen.ts:221, 262): reset the cache
/// when the alternate screen starts or stops.
pub fn clear_kitty_image_cache() {
    let mut guard = lock_kitty_image_cache();
    if let Some(cache) = guard.as_mut() {
        cache.clear();
    }
}

/// Test-only debug probe: records recent re-transmission decisions so flaky
/// failures can report the cache/registry state that caused them.
#[cfg(test)]
static RETRANSMIT_PROBE_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn record_retransmit_probe(entry: &str) {
    let mut guard = RETRANSMIT_PROBE_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.len() >= 64 {
        guard.remove(0);
    }
    guard.push(entry.to_string());
}

/// Read the recent re-transmission probe entries (test diagnostics).
#[cfg(test)]
pub(crate) fn retransmit_probe_log() -> Vec<String> {
    RETRANSMIT_PROBE_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// `this.uploadedKittyImages.size > 0` (tui-alt-screen.ts:1008): whether any
/// uploaded image is currently tracked (drives the full-redraw clear choice
/// between placement-only deletion and full image deletion).
pub fn kitty_image_cache_has_entries() -> bool {
    lock_kitty_image_cache()
        .as_ref()
        .is_some_and(|cache| !cache.map.is_empty())
}

/// Result of `prepareKittyScreen` (tui-alt-screen.ts:294): the prepared lines
/// plus the concatenated `deleteKittyImage` commands for evicted entries.
#[derive(Debug, Clone, PartialEq)]
pub struct PrepareKittyScreenResult {
    pub screen: Vec<String>,
    pub evicted_image_deletion: String,
}

/// `prepareKittyScreen` (tui-alt-screen.ts:294-342 @ 4181f66).
///
/// For every Kitty image line: a cache hit with the same transmission
/// generation emits the placement-only replacement line (zero re-upload); a
/// miss or a generation change emits the original line (re-transmission).
/// Every visible image id is touched (LRU). Offscreen entries are then
/// evicted oldest-first until all three quotas hold, emitting one
/// `deleteKittyImage` per eviction.
pub fn prepare_kitty_screen(screen: &[String]) -> PrepareKittyScreenResult {
    let mut guard = lock_kitty_image_cache();
    let cache = guard.get_or_insert_with(KittyImageCache::new);

    let mut visible_image_ids = HashSet::new();
    let mut lines = Vec::with_capacity(screen.len());
    for line in screen {
        match get_kitty_image_placement(line) {
            None => lines.push(line.clone()),
            Some(placement) => {
                visible_image_ids.insert(placement.image_id);
                let cached_image = cache.get(placement.image_id);
                // LRU touch before the hit/miss decision, exactly like
                // upstream (tui-alt-screen.ts:301-308).
                cache.set(
                    placement.image_id,
                    CachedKittyImage {
                        transmission_generation: placement.transmission_generation,
                        transmission_bytes: placement.transmission_bytes,
                        estimated_decoded_bytes: placement.estimated_decoded_bytes,
                    },
                );
                lines.push(match cached_image {
                    Some(cached)
                        if cached.transmission_generation == placement.transmission_generation =>
                    {
                        placement.replacement_line
                    }
                    _ => {
                        #[cfg(test)]
                        record_retransmit_probe(&format!(
                            "id={} cached_gen={:?} placement_gen={} cache_len={} thread={:?}",
                            placement.image_id,
                            cached_image.map(|c| c.transmission_generation),
                            placement.transmission_generation,
                            cache.entries_in_order().len(),
                            std::thread::current().name(),
                        ));
                        line.clone()
                    }
                });
            }
        }
    }

    // Quotas over offscreen (non-visible) entries only
    // (tui-alt-screen.ts:315-323).
    let mut cached_offscreen_image_count = 0usize;
    let mut cached_offscreen_transmission_bytes = 0usize;
    let mut cached_offscreen_decoded_bytes = 0f64;
    let entries = cache.entries_in_order();
    for (image_id, cached_image) in &entries {
        if visible_image_ids.contains(image_id) {
            continue;
        }
        cached_offscreen_image_count += 1;
        cached_offscreen_transmission_bytes += cached_image.transmission_bytes;
        cached_offscreen_decoded_bytes += cached_image.estimated_decoded_bytes;
    }

    // Evict offscreen entries oldest-first until all three quotas hold
    // (tui-alt-screen.ts:325-340). The snapshot preserves the JS `Map`
    // iteration order; the break-at-top check mirrors upstream exactly.
    let mut evicted_image_deletion = String::new();
    for (image_id, cached_image) in &entries {
        if cached_offscreen_image_count <= MAX_CACHED_OFFSCREEN_KITTY_IMAGES
            && cached_offscreen_transmission_bytes <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
            && cached_offscreen_decoded_bytes <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
        {
            break;
        }
        if visible_image_ids.contains(image_id) {
            continue;
        }
        evicted_image_deletion.push_str(&delete_kitty_image(*image_id));
        cache.remove(*image_id);
        cached_offscreen_image_count -= 1;
        cached_offscreen_transmission_bytes -= cached_image.transmission_bytes;
        cached_offscreen_decoded_bytes -= cached_image.estimated_decoded_bytes;
    }

    PrepareKittyScreenResult {
        screen: lines,
        evicted_image_deletion,
    }
}

#[cfg(test)]
mod tests {
    //! Ports of the upstream `TuiAltScreen` Kitty image cache tests (intent,
    //! not line-by-line — `packages/tui/test/tui-alt-screen.test.ts` tests
    //! 17-20 @ 4181f66): placement-only reuse, offscreen retention, LRU
    //! eviction, and the decoded-memory quota. The upstream tests drive a
    //! full TUI with a `RecordingTerminal`; here the screen arrays are fed to
    //! `prepare_kitty_screen` directly.

    use super::*;
    use crate::terminal_image::{
        allocate_image_id, encode_kitty, register_kitty_image_metadata, reset_kitty_image_metadata,
        KittyEncodeOptions, KittyImageMetadata, TEST_STATE_LOCK,
    };

    fn lock() -> MutexGuard<'static, ()> {
        TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_state() {
        clear_kitty_image_cache();
        reset_kitty_image_metadata();
    }

    /// Build a single-chunk Kitty image line with registered metadata,
    /// mirroring the upstream fixtures (`encodeKitty("AAAA", { columns: 2,
    /// rows: 1, imageId, moveCursor: false })` + `registerKittyImageMetadata`).
    fn kitty_line(image_id: u32, width_px: f64, height_px: f64) -> String {
        register_kitty_image_metadata(KittyImageMetadata {
            image_id,
            columns: 2,
            rows: 1,
            width_px,
            height_px,
        });
        encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(1),
                image_id: Some(image_id),
                move_cursor: Some(false),
            },
        )
    }

    #[test]
    fn test_reuses_moved_kitty_images_without_dropping_h_stack_siblings() {
        // tui-alt-screen.test.ts:575-623.
        let _guard = lock();
        reset_state();
        let id = allocate_image_id();
        register_kitty_image_metadata(KittyImageMetadata {
            image_id: id,
            columns: 2,
            rows: 1,
            width_px: 100.0,
            height_px: 50.0,
        });
        let transmission = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(1),
                image_id: Some(id),
                move_cursor: Some(false),
            },
        );
        let line = format!("left {transmission} right");

        // First render transmits the payload and keeps the sibling text.
        let first = prepare_kitty_screen(std::slice::from_ref(&line));
        assert!(first.screen[0].contains("\x1b_Ga=T"));
        assert!(first.screen[0].starts_with("left "));
        assert!(first.evicted_image_deletion.is_empty());

        // The moved image redraw reuses the upload: placement-only, siblings
        // preserved, no re-transmission.
        let second = prepare_kitty_screen(&[line]);
        assert!(second.screen[0].contains("\x1b_Ga=p,q=2"));
        assert!(!second.screen[0].contains("\x1b_Ga=T"));
        assert!(second.screen[0].starts_with("left \x1b_Ga=p,q=2"));
        assert!(second.screen[0].ends_with(" right"));
        assert!(second.evicted_image_deletion.is_empty());
    }

    #[test]
    fn test_retains_recently_offscreen_kitty_images_for_placement_only_reuse() {
        // tui-alt-screen.test.ts:625-663.
        let _guard = lock();
        reset_state();
        let image_id = 321u32;
        let image_line = kitty_line(image_id, 100.0, 50.0);
        let screen = vec![image_line.clone(), "after".to_string()];

        let first = prepare_kitty_screen(&screen);
        assert!(first.screen[0].contains("\x1b_Ga=T"));

        // Scroll the image offscreen: it stays cached (all quotas hold).
        let offscreen = prepare_kitty_screen(&["after".to_string()]);
        assert!(offscreen.evicted_image_deletion.is_empty());

        // Scroll back: placement-only reuse, no re-upload, no eviction.
        let reentry = prepare_kitty_screen(&screen);
        assert!(reentry.screen[0].contains("\x1b_Ga=p,q=2"));
        assert!(!reentry.screen[0].contains("\x1b_Ga=T"));
        assert!(!reentry
            .evicted_image_deletion
            .contains(&format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")));
    }

    #[test]
    fn test_evicts_the_least_recently_visible_kitty_image_when_the_cache_is_full() {
        // tui-alt-screen.test.ts:665-710.
        let _guard = lock();
        reset_state();
        let first_image_id = 500u32;
        let image_lines: Vec<String> = (0..18)
            .map(|index| kitty_line(first_image_id + index, 100.0, 50.0))
            .collect();

        // One visible image per frame, scrolling through all 18: once the
        // 17th offscreen entry appears, the least recently visible image
        // (500) is evicted.
        let mut evictions = String::new();
        for line in &image_lines {
            let result = prepare_kitty_screen(std::slice::from_ref(line));
            evictions.push_str(&result.evicted_image_deletion);
        }
        assert!(evictions.contains(&delete_kitty_image(first_image_id)));

        // Scrolling back to the evicted image re-transmits it.
        let reentry = prepare_kitty_screen(std::slice::from_ref(&image_lines[0]));
        assert!(reentry.screen[0].contains("\x1b_Ga=T"));
    }

    #[test]
    fn test_evicts_offscreen_kitty_images_when_decoded_raster_memory_exceeds_the_cache_quota() {
        // tui-alt-screen.test.ts:712-747.
        let _guard = lock();
        reset_state();
        let first_image_id = 600u32;
        // 3840x2160 -> 33,177,600 decoded bytes each; three offscreen exceed
        // the 64 MiB quota, so the oldest (600) is evicted.
        let image_lines: Vec<String> = (0..4)
            .map(|index| kitty_line(first_image_id + index, 3840.0, 2160.0))
            .collect();

        let mut evictions = String::new();
        for line in &image_lines {
            let result = prepare_kitty_screen(std::slice::from_ref(line));
            evictions.push_str(&result.evicted_image_deletion);
        }
        assert!(evictions.contains(&delete_kitty_image(first_image_id)));
    }

    #[test]
    fn test_retransmits_when_the_transmission_generation_changes() {
        let _guard = lock();
        reset_state();
        let image_id = allocate_image_id();
        let line = kitty_line(image_id, 100.0, 50.0);

        let first = prepare_kitty_screen(std::slice::from_ref(&line));
        assert!(first.screen[0].contains("\x1b_Ga=T"));

        // Re-registering the same id (fresh content) bumps the transmission
        // generation: the cached entry is stale and the line re-transmits.
        register_kitty_image_metadata(KittyImageMetadata {
            image_id,
            columns: 2,
            rows: 1,
            width_px: 100.0,
            height_px: 50.0,
        });
        let second = prepare_kitty_screen(std::slice::from_ref(&line));
        assert!(second.screen[0].contains("\x1b_Ga=T"));
        assert!(!second.screen[0].contains("\x1b_Ga=p,q=2"));

        // The refreshed entry is reused placement-only from now on.
        let third = prepare_kitty_screen(&[line]);
        assert!(third.screen[0].contains("\x1b_Ga=p,q=2"));
        assert!(!third.screen[0].contains("\x1b_Ga=T"));
    }

    #[test]
    fn test_plain_lines_pass_through_and_do_not_evict() {
        let _guard = lock();
        reset_state();
        let id = allocate_image_id();
        let image_line = kitty_line(id, 100.0, 50.0);
        let first = prepare_kitty_screen(std::slice::from_ref(&image_line));
        assert!(first.screen[0].contains("\x1b_Ga=T"));

        let plain = prepare_kitty_screen(&["hello".to_string()]);
        assert_eq!(plain.screen, vec!["hello".to_string()]);
        assert!(plain.evicted_image_deletion.is_empty());

        let reentry = prepare_kitty_screen(&[image_line]);
        assert!(reentry.screen[0].contains("\x1b_Ga=p,q=2"));
    }
}
