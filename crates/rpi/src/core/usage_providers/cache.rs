//! Last-success usage cache (V16-05 FR-A R1/R3; V18-10 identity).
//!
//! The host keeps the most recent successful envelope per credential
//! identity with its fetch timestamp. A fresh entry answers
//! `fetch(provider, force=false)` without running the script (TTL-bounded);
//! after a failed fetch the stale entry is still returned so the consumer
//! never sees the value disappear.
//!
//! V18-10: the key is `(provider, generation, effectiveOrigin)` for a
//! first-party provider, so a changed account, token rotation or origin
//! change can never read another identity's cached quota. The cache is
//! process/session memory only and never persists a secret.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::UsageEnvelope;

/// Default cache TTL: a non-forced fetch inside this window returns the
/// cached envelope without re-running the script (V16-05 §7.2).
pub const DEFAULT_USAGE_CACHE_TTL_MS: u64 = 60_000;

/// Old identities are swept on every store after this age.
pub const DEFAULT_USAGE_CACHE_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1_000;

/// Cache identity. Legacy third-party providers have no host-proven
/// identity, so both identity parts are empty and the old per-provider
/// behavior is preserved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UsageCacheKey {
    provider: String,
    generation: String,
    origin: String,
}

impl UsageCacheKey {
    pub fn new(
        provider: impl Into<String>,
        generation: impl Into<String>,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            generation: generation.into(),
            origin: origin.into(),
        }
    }

    pub fn legacy(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            generation: String::new(),
            origin: String::new(),
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether this key carries a host-proven credential identity.
    pub fn has_identity(&self) -> bool {
        !self.generation.is_empty() && !self.origin.is_empty()
    }
}

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
    max_age: Duration,
    entries: Mutex<HashMap<UsageCacheKey, UsageCacheEntry>>,
}

impl UsageCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_max_age(ttl, Duration::from_millis(DEFAULT_USAGE_CACHE_MAX_AGE_MS))
    }

    pub fn with_max_age(ttl: Duration, max_age: Duration) -> Self {
        Self {
            ttl,
            max_age,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Test-only TTL override (the registry's stale-branch tests need to
    /// expire an existing entry without dropping its identity).
    #[cfg(test)]
    pub(crate) fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }

    /// A TTL-fresh entry for `key`, if any.
    pub fn get_fresh(&self, key: &UsageCacheKey) -> Option<UsageEnvelope> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(key)
            .filter(|entry| entry.is_fresh(self.ttl))
            .map(|entry| entry.envelope.clone())
    }

    /// The last successful entry for `key` regardless of age (the failure
    /// fallback).
    pub fn get_last(&self, key: &UsageCacheKey) -> Option<UsageEnvelope> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(|entry| entry.envelope.clone())
    }

    /// Whether the last entry for `key` is still inside the TTL.
    pub fn is_fresh(&self, key: &UsageCacheKey) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .is_some_and(|entry| entry.is_fresh(self.ttl))
    }

    /// Record a success, replacing any previous entry for the same identity
    /// and sweeping identities older than the maximum age.
    pub fn store(&self, key: &UsageCacheKey, envelope: UsageEnvelope) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.fetched_at.elapsed() < self.max_age);
        entries.insert(
            key.clone(),
            UsageCacheEntry {
                envelope,
                fetched_at: Instant::now(),
            },
        );
    }

    /// Drop every identity of one provider (used when the provider is
    /// re-registered or its resolution changes).
    pub fn remove_provider(&self, provider: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| key.provider() != provider);
    }

    /// Number of live entries (tests/diagnostics).
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
            windows: None,
            ledgers: None,
            quota: None,
            used: None,
            reset_at: None,
            display_text: display.to_owned(),
        }
    }

    #[test]
    fn fresh_entries_answer_within_ttl() {
        let cache = UsageCache::new(Duration::from_millis(60_000));
        cache.store(&UsageCacheKey::legacy("a"), envelope("a", "a: ok"));
        assert_eq!(
            cache
                .get_fresh(&UsageCacheKey::legacy("a"))
                .map(|e| e.display_text),
            Some("a: ok".to_owned())
        );
        assert!(cache.get_fresh(&UsageCacheKey::legacy("missing")).is_none());
    }

    #[test]
    fn expired_entries_are_not_fresh_but_still_last() {
        let cache = UsageCache::new(Duration::from_millis(0));
        cache.store(&UsageCacheKey::legacy("a"), envelope("a", "a: ok"));
        std::thread::sleep(Duration::from_millis(1));
        assert!(cache.get_fresh(&UsageCacheKey::legacy("a")).is_none());
        assert_eq!(
            cache
                .get_last(&UsageCacheKey::legacy("a"))
                .map(|e| e.display_text),
            Some("a: ok".to_owned())
        );
        assert!(!cache.is_fresh(&UsageCacheKey::legacy("a")));
    }

    #[test]
    fn identity_parts_isolate_and_re_registration_clears() {
        let cache = UsageCache::new(Duration::from_millis(60_000));
        let first = UsageCacheKey::new("p", "gen-1", "https://api.example.com");
        let second = UsageCacheKey::new("p", "gen-2", "https://api.example.com");
        cache.store(&first, envelope("p", "first"));
        assert!(cache.get_last(&second).is_none(), "identity mismatch");
        cache.store(&second, envelope("p", "second"));
        assert_eq!(cache.len(), 2);
        cache.remove_provider("p");
        assert!(cache.get_last(&first).is_none());
        assert!(cache.get_last(&second).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn old_identities_are_swept_on_store() {
        let cache =
            UsageCache::with_max_age(Duration::from_millis(60_000), Duration::from_millis(0));
        cache.store(
            &UsageCacheKey::new("p", "gen-1", "https://api.example.com"),
            envelope("p", "first"),
        );
        std::thread::sleep(Duration::from_millis(1));
        cache.store(
            &UsageCacheKey::new("p", "gen-2", "https://api.example.com"),
            envelope("p", "second"),
        );
        assert_eq!(cache.len(), 1);
        assert!(
            cache
                .get_last(&UsageCacheKey::new("p", "gen-1", "https://api.example.com"))
                .is_none()
        );
    }

    #[test]
    fn store_replaces_the_same_identity() {
        let cache = UsageCache::new(Duration::from_millis(60_000));
        let key = UsageCacheKey::legacy("a");
        cache.store(&key, envelope("a", "first"));
        cache.store(&key, envelope("a", "second"));
        assert_eq!(
            cache.get_last(&key).map(|e| e.display_text),
            Some("second".to_owned())
        );
        assert_eq!(cache.len(), 1);
    }
}
