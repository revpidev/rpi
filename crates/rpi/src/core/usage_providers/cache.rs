//! Last-success usage envelope cache (V16-05 FR-A R1/R3).
//!
//! The host keeps the most recent *successful* envelope per provider with its
//! fetch timestamp. A fresh entry answers `fetch(provider, force=false)`
//! without running the script (TTL-bounded); after a failed fetch the stale
//! entry is still returned so the consumer never sees the value disappear.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::UsageEnvelope;

/// Default cache TTL: a non-forced fetch inside this window returns the
/// cached envelope without re-running the script (V16-05 §7.2).
pub const DEFAULT_USAGE_CACHE_TTL_MS: u64 = 60_000;

/// One cached success.
#[derive(Debug, Clone)]
pub struct UsageCacheEntry {
    pub envelope: UsageEnvelope,
    fetched_at: Instant,
}

impl UsageCacheEntry {
    /// Whether the entry is younger than the TTL.
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

/// Per-registry cache (one instance per session).
#[derive(Debug)]
pub struct UsageCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, UsageCacheEntry>>,
}

impl UsageCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// A TTL-fresh entry for `provider`, if any.
    pub fn get_fresh(&self, provider: &str) -> Option<UsageEnvelope> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(provider)
            .filter(|entry| entry.is_fresh(self.ttl))
            .map(|entry| entry.envelope.clone())
    }

    /// The last successful entry regardless of age (the failure fallback).
    pub fn get_last(&self, provider: &str) -> Option<UsageEnvelope> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
            .map(|entry| entry.envelope.clone())
    }

    /// Record a success, replacing any previous entry.
    pub fn store(&self, provider: &str, envelope: UsageEnvelope) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                provider.to_owned(),
                UsageCacheEntry {
                    envelope,
                    fetched_at: Instant::now(),
                },
            );
    }

    /// Drop an entry (used when the provider is re-registered).
    pub fn remove(&self, provider: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn envelope(provider: &str, display: &str) -> UsageEnvelope {
        UsageEnvelope {
            schema_version: 1,
            provider: provider.to_owned(),
            plan: None,
            balance: None,
            quota: None,
            used: None,
            reset_at: None,
            display_text: display.to_owned(),
        }
    }

    #[test]
    fn fresh_entries_answer_within_ttl() {
        let cache = UsageCache::new(Duration::from_millis(60_000));
        cache.store("a", envelope("a", "a: ok"));
        assert_eq!(
            cache.get_fresh("a").map(|e| e.display_text),
            Some("a: ok".to_owned())
        );
        assert!(cache.get_fresh("missing").is_none());
    }

    #[test]
    fn expired_entries_are_not_fresh_but_still_last() {
        let cache = UsageCache::new(Duration::from_millis(0));
        cache.store("a", envelope("a", "a: ok"));
        std::thread::sleep(Duration::from_millis(1));
        assert!(cache.get_fresh("a").is_none());
        assert_eq!(
            cache.get_last("a").map(|e| e.display_text),
            Some("a: ok".to_owned())
        );
    }

    #[test]
    fn store_replaces_and_remove_drops() {
        let cache = UsageCache::new(Duration::from_millis(60_000));
        cache.store("a", envelope("a", "first"));
        cache.store("a", envelope("a", "second"));
        assert_eq!(
            cache.get_last("a").map(|e| e.display_text),
            Some("second".to_owned())
        );
        cache.remove("a");
        assert!(cache.get_last("a").is_none());
    }
}
