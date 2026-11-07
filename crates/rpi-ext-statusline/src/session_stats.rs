//! Absolute session statistics cache (TE52 R20 §4.2 / 04 §3.3).
//!
//! The plugin never parses the transcript and never accumulates entries by
//! message id. `ctx.sessionStats` is the host-authoritative absolute
//! snapshot: identity, RFC3339 `startedAt`, `entryCount`, the persisted
//! user/assistant message counts, the four token components and cost, all
//! read under one `SessionManager` lock.
//!
//! Guards (frozen by ADR-0040):
//! - every session lifecycle event bumps the session generation, so a reply
//!   from the previous session can never be applied;
//! - every read takes a new request generation and only the reply for the
//!   latest issued read may replace the snapshot (A(entryCount=10) arriving
//!   after B(entryCount=11) is dropped);
//! - within one session identity, a reply whose `entryCount` is lower than
//!   the applied value is dropped (no stale overwrite);
//! - a changed session id/path is a new identity and resets the entry guard.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The four token components (camelCase on the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenStats {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

/// One `ctx.sessionStats` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub entry_count: u64,
    /// Persisted user messages (one per conversation turn).
    #[serde(default)]
    pub user_messages: u64,
    /// Persisted assistant messages (one per model output/step).
    #[serde(default)]
    pub assistant_messages: u64,
    #[serde(default)]
    pub tokens: TokenStats,
    #[serde(default)]
    pub cost: f64,
}

impl SessionStats {
    /// `startedAt` from the session header (RFC3339), never the process
    /// start.
    pub fn started_at_utc(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(self.started_at.trim())
            .ok()
            .map(|value| value.with_timezone(&Utc))
    }
}

/// One read's identity: only a reply carrying both generations may replace
/// the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestToken {
    pub session_generation: u64,
    pub request_generation: u64,
}

/// The absolute snapshot cache + generation guards.
#[derive(Debug, Default)]
pub struct SessionStatsCache {
    session_generation: u64,
    request_generation: u64,
    applied_request_generation: u64,
    applied_entry_count: u64,
    identity: Option<(String, Option<String>)>,
    snapshot: Option<SessionStats>,
}

impl SessionStatsCache {
    /// A session lifecycle event (startup/new/resume/fork/reload): start a new
    /// session generation and drop the previous identity/snapshot. The next
    /// absolute reply repopulates everything; no entry is re-added.
    pub fn begin_session(&mut self, _reason: Option<&str>) -> u64 {
        self.session_generation = self.session_generation.wrapping_add(1);
        self.request_generation = 0;
        self.applied_request_generation = 0;
        self.applied_entry_count = 0;
        self.identity = None;
        self.snapshot = None;
        self.session_generation
    }

    /// Current session generation (diagnostics/tests).
    pub fn session_generation(&self) -> u64 {
        self.session_generation
    }

    /// Start one read of `ctx.sessionStats`.
    pub fn begin_request(&mut self) -> RequestToken {
        self.request_generation = self.request_generation.wrapping_add(1);
        RequestToken {
            session_generation: self.session_generation,
            request_generation: self.request_generation,
        }
    }

    /// Apply a reply. Returns whether the snapshot was replaced.
    pub fn apply(&mut self, token: RequestToken, stats: SessionStats) -> bool {
        if token.session_generation != self.session_generation {
            return false;
        }
        // Only the latest issued read may win; an already-applied generation
        // is a duplicate and is ignored.
        if token.request_generation != self.request_generation
            || token.request_generation <= self.applied_request_generation
        {
            return false;
        }
        let identity = (stats.session_id.clone(), stats.session_file.clone());
        if self.identity.as_ref() != Some(&identity) {
            // New session identity (resume/fork/reload with a different
            // file): the entry guard starts over for this identity.
            self.applied_entry_count = 0;
            self.identity = Some(identity);
        }
        if stats.entry_count < self.applied_entry_count {
            return false;
        }
        self.applied_entry_count = stats.entry_count;
        self.applied_request_generation = token.request_generation;
        self.snapshot = Some(stats);
        true
    }

    pub fn snapshot(&self) -> Option<&SessionStats> {
        self.snapshot.as_ref()
    }

    /// Cheap change fingerprint for the refresh poll.
    pub fn fingerprint(&self) -> String {
        match &self.snapshot {
            None => "none".to_owned(),
            Some(stats) => format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
                stats.session_id,
                stats.entry_count,
                stats.user_messages,
                stats.assistant_messages,
                stats.tokens.input,
                stats.tokens.output,
                stats.tokens.cache_read,
                stats.tokens.cache_write,
                stats.cost,
                stats.session_file
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(entry_count: u64) -> SessionStats {
        SessionStats {
            session_id: "s1".into(),
            session_file: Some("/sessions/s1.jsonl".into()),
            started_at: "2026-10-27T10:00:00Z".into(),
            entry_count,
            user_messages: 2,
            assistant_messages: 3,
            tokens: TokenStats {
                input: 10,
                output: 20,
                cache_read: 30,
                cache_write: 40,
            },
            cost: 0.5,
        }
    }

    #[test]
    fn applies_the_latest_reply_and_rejects_a_late_older_one() {
        let mut cache = SessionStatsCache::default();
        let first = cache.begin_request();
        let second = cache.begin_request();
        // B (entryCount=11) returns first.
        assert!(cache.apply(second, stats(11)));
        assert_eq!(cache.snapshot().map(|s| s.entry_count), Some(11));
        // A (entryCount=10) arrives late with an older request generation.
        assert!(!cache.apply(first, stats(10)));
        assert_eq!(cache.snapshot().map(|s| s.entry_count), Some(11));
    }

    #[test]
    fn drops_a_lower_entry_count_within_the_same_identity() {
        let mut cache = SessionStatsCache::default();
        let first = cache.begin_request();
        assert!(cache.apply(first, stats(11)));
        // Even a newer request carrying an older count is dropped.
        let second = cache.begin_request();
        assert!(!cache.apply(second, stats(10)));
        assert_eq!(cache.snapshot().map(|s| s.entry_count), Some(11));
        // Equal count is a valid absolute replacement.
        let third = cache.begin_request();
        let mut same = stats(11);
        same.cost = 0.75;
        assert!(cache.apply(third, same));
        assert_eq!(cache.snapshot().map(|s| s.cost), Some(0.75));
    }

    #[test]
    fn session_lifecycle_invalidates_previous_replies() {
        let mut cache = SessionStatsCache::default();
        let old = cache.begin_request();
        cache.begin_session(Some("new"));
        assert!(!cache.apply(old, stats(3)));
        assert_eq!(cache.snapshot(), None);
        let fresh = cache.begin_request();
        let mut next = stats(0);
        next.session_id = "s2".into();
        next.session_file = None;
        assert!(cache.apply(fresh, next));
        assert_eq!(cache.snapshot().map(|s| s.session_id.as_str()), Some("s2"));
        // A changed identity resets the entry guard (an in-memory session can
        // restart at a lower count after resume).
        let third = cache.begin_request();
        let mut other = stats(2);
        other.session_id = "s3".into();
        assert!(cache.apply(third, other));
        assert_eq!(cache.snapshot().map(|s| s.entry_count), Some(2));
    }

    #[test]
    fn duplicate_apply_of_the_same_token_is_rejected() {
        let mut cache = SessionStatsCache::default();
        let token = cache.begin_request();
        assert!(cache.apply(token, stats(1)));
        assert!(!cache.apply(token, stats(2)));
        assert_eq!(cache.snapshot().map(|s| s.entry_count), Some(1));
    }

    #[test]
    fn started_at_parses_rfc3339() {
        let parsed = stats(1).started_at_utc().expect("rfc3339");
        assert_eq!(parsed.to_rfc3339(), "2026-10-27T10:00:00+00:00");
        // Offsets are normalized to UTC; malformed values are None.
        let mut offset = stats(1);
        offset.started_at = "2026-10-27T12:00:00+02:00".into();
        assert_eq!(
            offset.started_at_utc().map(|value| value.to_rfc3339()),
            Some("2026-10-27T10:00:00+00:00".to_owned())
        );
        offset.started_at = "not-a-date".into();
        assert_eq!(offset.started_at_utc(), None);
    }

    #[test]
    fn wire_shape_matches_the_host_contract() {
        let parsed: SessionStats = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "sessionFile": "/sessions/s1.jsonl",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 12,
            "userMessages": 4,
            "assistantMessages": 7,
            "tokens": {"input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40},
            "cost": 1.25
        }))
        .expect("host shape parses");
        assert_eq!(parsed.session_id, "s1");
        assert_eq!(parsed.session_file.as_deref(), Some("/sessions/s1.jsonl"));
        assert_eq!(parsed.entry_count, 12);
        assert_eq!(parsed.user_messages, 4);
        assert_eq!(parsed.assistant_messages, 7);
        assert_eq!(parsed.tokens.cache_read, 30);
        assert_eq!(parsed.tokens.cache_write, 40);
        assert_eq!(parsed.cost, 1.25);
        // In-memory sessions omit the file.
        let in_memory: SessionStats = serde_json::from_value(serde_json::json!({
            "sessionId": "s2",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 0,
            "tokens": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "cost": 0.0
        }))
        .expect("in-memory shape");
        assert_eq!(in_memory.session_file, None);
        // Older hosts predate the count keys: they default to zero instead
        // of failing the whole snapshot.
        let legacy: SessionStats = serde_json::from_value(serde_json::json!({
            "sessionId": "s3",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 5,
            "tokens": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4},
            "cost": 0.0
        }))
        .expect("legacy shape");
        assert_eq!(legacy.user_messages, 0);
        assert_eq!(legacy.assistant_messages, 0);
    }
}
