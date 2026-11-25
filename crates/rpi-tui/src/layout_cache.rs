//! Cross-frame layout render cache (V19-06 FR-A, ADR-0049 decision 4).
//!
//! The layout engine's frame-local cache dedups renders inside one frame;
//! this cache keeps the rendered lines of trusted components across frames.
//! An entry is keyed by the component's `Arc` address (a hash locator only),
//! width, content revision and the renderer's frame generation, and holds a
//! strong reference to its owner: a hit requires the full key AND
//! `Arc::ptr_eq`, and because the owner cannot be freed while the entry
//! lives, a new component can never be served the lines of a released one
//! that happened to get the same address (ABA). Components without a trusted
//! [`Component::cache_revision`] are never stored.

use std::collections::HashMap;
use std::sync::Arc;

use crate::tui::SharedComponent;

/// Entry capacity (each entry is one component at one width).
pub const MAX_CACHE_ENTRIES: usize = 512;
/// Retained line capacity across entries.
pub const MAX_CACHE_LINES: usize = 32_768;

struct CacheEntry {
    owner: SharedComponent,
    width: usize,
    revision: u64,
    generation: u64,
    lines: Arc<[String]>,
    touch: u64,
}

/// Counters for tests and the performance report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRenderCacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Renders of untrusted components (never cached).
    pub untrusted_renders: u64,
    pub invalidations: u64,
    pub evictions: u64,
    pub retained_entries: usize,
    pub retained_lines: usize,
}

#[derive(Default)]
pub struct LayoutRenderCache {
    /// By `Arc::as_ptr` address; each bucket holds the entries of the
    /// components seen at that address (normally one).
    entries: HashMap<usize, Vec<CacheEntry>>,
    generation: u64,
    clock: u64,
    lines: usize,
    stats: LayoutRenderCacheStats,
}

impl LayoutRenderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Theme/transform/capability invalidation and renderer swaps: every
    /// retained line belongs to the previous generation.
    pub fn invalidate(&mut self) {
        if !self.entries.is_empty() {
            self.stats.invalidations += 1;
        }
        self.entries.clear();
        self.lines = 0;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Current frame generation (part of every key).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Records a render of an untrusted component (never cached).
    pub fn note_untrusted_render(&mut self) {
        self.stats.untrusted_renders += 1;
    }

    /// Drops the entries of a component that left the tree (removal/swap).
    pub fn remove_component(&mut self, component: &SharedComponent) {
        let key = Arc::as_ptr(component) as usize;
        if let Some(bucket) = self.entries.get_mut(&key) {
            bucket.retain(|entry| !Arc::ptr_eq(&entry.owner, component));
            if bucket.is_empty() {
                self.entries.remove(&key);
            }
            self.recount();
        }
    }

    /// Lines of `component` when an entry matches the owner identity, width,
    /// revision and generation.
    pub fn get(
        &mut self,
        component: &SharedComponent,
        width: usize,
        revision: u64,
    ) -> Option<Arc<[String]>> {
        let key = Arc::as_ptr(component) as usize;
        let generation = self.generation;
        let clock = self.tick();
        let bucket = self.entries.get_mut(&key)?;
        let entry = bucket.iter_mut().find(|entry| {
            Arc::ptr_eq(&entry.owner, component)
                && entry.width == width
                && entry.revision == revision
                && entry.generation == generation
        })?;
        entry.touch = clock;
        self.stats.hits += 1;
        Some(entry.lines.clone())
    }

    /// Stores the lines rendered for a trusted component at `width`.
    pub fn insert(
        &mut self,
        owner: SharedComponent,
        width: usize,
        revision: u64,
        lines: Arc<[String]>,
    ) {
        self.stats.misses += 1;
        let key = Arc::as_ptr(&owner) as usize;
        let touch = self.tick();
        let bucket = self.entries.entry(key).or_default();
        // One entry per (owner, width): a stale revision replaces in place.
        if let Some(entry) = bucket.iter_mut().find(|entry| {
            Arc::ptr_eq(&entry.owner, &owner)
                && entry.width == width
                && entry.generation == self.generation
        }) {
            self.lines = self.lines.saturating_sub(entry.lines.len());
            entry.revision = revision;
            entry.lines = lines.clone();
            entry.touch = touch;
        } else {
            bucket.push(CacheEntry {
                owner,
                width,
                revision,
                generation: self.generation,
                lines: lines.clone(),
                touch,
            });
        }
        self.lines += lines.len();
        self.evict();
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn len(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    fn recount(&mut self) {
        self.lines = self
            .entries
            .values()
            .flatten()
            .map(|entry| entry.lines.len())
            .sum();
    }

    /// Least-recently-used eviction down to both capacities.
    fn evict(&mut self) {
        if self.len() <= MAX_CACHE_ENTRIES && self.lines <= MAX_CACHE_LINES {
            return;
        }
        let mut order: Vec<(u64, usize)> = self
            .entries
            .iter()
            .flat_map(|(key, bucket)| bucket.iter().map(move |entry| (entry.touch, *key)))
            .collect();
        order.sort_unstable();
        for (touch, key) in order {
            if self.len() <= MAX_CACHE_ENTRIES && self.lines <= MAX_CACHE_LINES {
                break;
            }
            let Some(bucket) = self.entries.get_mut(&key) else {
                continue;
            };
            // Touches are unique per entry (monotonic clock).
            if let Some(position) = bucket.iter().position(|entry| entry.touch == touch) {
                let entry = bucket.remove(position);
                self.lines = self.lines.saturating_sub(entry.lines.len());
                self.stats.evictions += 1;
            }
            if bucket.is_empty() {
                self.entries.remove(&key);
            }
        }
    }

    pub fn stats(&self) -> LayoutRenderCacheStats {
        let mut stats = self.stats;
        stats.retained_entries = self.entries.values().map(Vec::len).sum();
        stats.retained_lines = self.lines;
        stats
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::components::spacer::Spacer;
    use crate::tui::{Component, shared_component};

    #[test]
    fn hit_requires_identity_width_revision_and_generation() {
        let mut cache = LayoutRenderCache::new();
        let spacer = shared_component(Spacer::new(2));
        let lines: Arc<[String]> = vec![String::new(), String::new()].into();
        assert!(cache.get(&spacer, 10, 0).is_none());
        cache.insert(spacer.clone(), 10, 0, lines.clone());
        let hit = cache.get(&spacer, 10, 0).expect("stored entry");
        assert!(Arc::ptr_eq(&hit, &lines), "a hit clones the Arc only");
        assert!(
            cache.get(&spacer, 11, 0).is_none(),
            "width is part of the key"
        );
        assert!(
            cache.get(&spacer, 10, 1).is_none(),
            "revision is part of the key"
        );
        cache.invalidate();
        assert!(
            cache.get(&spacer, 10, 0).is_none(),
            "generation is part of the key"
        );
    }

    #[test]
    fn a_new_component_at_the_same_address_never_hits_the_old_lines() {
        // The ABA case, modeled directly: the bucket that the hash locator
        // resolves for the NEW component still holds an entry owned by the
        // OLD component (same address, same width/revision/generation). The
        // full key matches; only `Arc::ptr_eq` can rule the hit out.
        let mut cache = LayoutRenderCache::new();
        let old = shared_component(Spacer::new(1));
        let new = shared_component(Spacer::new(1));
        cache.insert(old.clone(), 8, 0, vec!["old".to_string()].into());
        // Move the old entry under the new component's address.
        let old_key = Arc::as_ptr(&old) as usize;
        let new_key = Arc::as_ptr(&new) as usize;
        if old_key != new_key {
            let bucket = cache.entries.remove(&old_key);
            if let Some(bucket) = bucket {
                cache.entries.insert(new_key, bucket);
            }
        }
        assert!(
            cache.get(&new, 8, 0).is_none(),
            "identity must veto the hit"
        );
        assert_eq!(cache.stats().hits, 0);
    }

    #[test]
    fn removed_components_are_forgotten_and_untrusted_components_are_not_stored() {
        struct Untrusted {
            renders: Arc<AtomicU64>,
        }
        impl Component for Untrusted {
            fn render(&self, _width: usize) -> Vec<String> {
                self.renders.fetch_add(1, Ordering::Relaxed);
                vec!["x".to_string()]
            }
        }
        let mut cache = LayoutRenderCache::new();
        let spacer = shared_component(Spacer::new(1));
        cache.insert(spacer.clone(), 8, 0, vec![String::new()].into());
        assert_eq!(cache.stats().retained_entries, 1);
        cache.remove_component(&spacer);
        assert_eq!(cache.stats().retained_entries, 0);
        assert!(cache.get(&spacer, 8, 0).is_none());
        // Nothing is stored for an untrusted component because nothing calls
        // `insert` for it (the layout engine checks `cache_revision` first).
        let renders = Arc::new(AtomicU64::new(0));
        let untrusted = shared_component(Untrusted {
            renders: Arc::clone(&renders),
        });
        assert_eq!(
            untrusted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cache_revision(),
            None
        );
    }

    #[test]
    fn entries_are_bounded_by_an_lru() {
        let mut cache = LayoutRenderCache::new();
        let components: Vec<SharedComponent> = (0..MAX_CACHE_ENTRIES + 8)
            .map(|index| shared_component(Spacer::new(1 + index % 3)))
            .collect();
        for (index, component) in components.iter().enumerate() {
            let lines: Arc<[String]> = (0..=index % 3).map(|_| String::new()).collect();
            cache.insert(component.clone(), 8, 0, lines);
        }
        let stats = cache.stats();
        assert!(stats.retained_entries <= MAX_CACHE_ENTRIES);
        assert!(stats.evictions >= 8);
        // The most recently inserted entries survive; the oldest are gone.
        assert!(cache.get(&components[components.len() - 1], 8, 0).is_some());
        assert!(cache.get(&components[0], 8, 0).is_none());
    }
}
