//! Model exclusion store (#1318/#1439, `src/runs/shared/model-exclusions.ts`
//! @ 0fc0eebb): a small TTL-backed "recently failed" cache consulted during
//! model-candidate assembly. A retryable provider failure records an
//! exclusion for the failing model (or provider); later launches skip
//! excluded candidates for the TTL window instead of re-burning a run on a
//! known-bad model.
//!
//! Store layout: version-1 JSON at the plugin temp root
//! (`RPI_MODEL_EXCLUSIONS_PATH` overrides, tests point it at scratch files),
//! ≤200 entries deduplicated by `(provider, modelId)`, auth-flavored entries
//! invalidated when the auth store is newer than their recording. Writes are
//! immediate (`flushPersist`) — the upstream debounce is a Node-lifetime
//! optimization, not behavior.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use regex::Regex;
use serde_json::{json, Value};

/// `DEFAULT_MODEL_EXCLUSION_TTL_MS` (24h).
pub const DEFAULT_MODEL_EXCLUSION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// `MAX_MODEL_EXCLUSION_TTL_MS` (config ceiling).
pub const MAX_MODEL_EXCLUSION_TTL_MS: u64 = 8_000_000_000_000_000;
/// Entry cap (`if (exclusions.length > 200)`).
const MAX_ENTRIES: usize = 200;

/// Env override for the store path (tests).
pub const EXCLUSIONS_PATH_ENV: &str = "RPI_MODEL_EXCLUSIONS_PATH";

/// Upstream catalog bounds — `MODEL_EXCLUSION_DIAGNOSTIC_MAX_LENGTH`
/// (240) and `MODEL_EXCLUSION_DIAGNOSTIC_MAX_ENTRIES` (20),
/// model-fallback.ts:293-294.
pub const MODEL_EXCLUSION_DIAGNOSTIC_MAX_LENGTH: usize = 240;
pub const MODEL_EXCLUSION_DIAGNOSTIC_MAX_ENTRIES: usize = 20;

/// `redactSecretValues` (permissions.ts:14-18 @ 0fc0eebb, applied to every
/// exclusion diagnostic upstream via `sanitizeModelExclusionDiagnostic` /
/// `throwForExplicitModelExclusion`): bearer tokens and well-known key
/// shapes become `[redacted]`.
//
// G4 不变式：静态字面量模式，编译有效性由 `sanitize_diagnostic_redacts_and_caps`
// 用例固定（Bearer/sk- 两形态断言 [redacted]）；`Regex::new` 对固定字面量
// 不会失败，失败即程序性错误——安全面不设静默降级分支（复核第 2 轮 P1）。
static SECRET_VALUE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:Bearer\s+\S+|(?:sk|ghp|github_pat|xox[baprs])[-_A-Za-z0-9]{8,})\b")
        .expect("SECRET_VALUE static literal is a valid regex")
});

/// See [`SECRET_VALUE`].
fn redact_secret_values(value: &str) -> String {
    SECRET_VALUE.replace_all(value, "[redacted]").to_string()
}

/// `sanitizeModelExclusionDiagnostic` (model-fallback.ts:296-301 @
/// 0fc0eebb): control characters (incl. U+2028/U+2029) collapse to spaces,
/// the trimmed value (or the fallback when empty) passes secret redaction,
/// then caps at [`MODEL_EXCLUSION_DIAGNOSTIC_MAX_LENGTH`] (upstream slices
/// UTF-16 units; `chars()` keeps the cap at most as long).
pub fn sanitize_diagnostic(value: &str, fallback: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|c| {
            if (c as u32) <= 0x1f || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let trimmed = normalized.trim();
    let source = if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    };
    let redacted = redact_secret_values(source);
    if redacted.chars().count() <= MODEL_EXCLUSION_DIAGNOSTIC_MAX_LENGTH {
        redacted
    } else {
        redacted
            .chars()
            .take(MODEL_EXCLUSION_DIAGNOSTIC_MAX_LENGTH)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelExclusion {
    pub model_id: Option<String>,
    pub provider: Option<String>,
    pub reason: String,
    pub recorded_at: u64,
    pub expires_at: u64,
}

#[derive(Default)]
struct Store {
    exclusions: Vec<ModelExclusion>,
    loaded: bool,
    default_ttl_ms: u64,
    /// Path the current in-memory snapshot was loaded from — a repointed
    /// `RPI_MODEL_EXCLUSIONS_PATH` (upstream: "so tests can point the store
    /// at an isolated location after module load") reloads from the new
    /// path instead of serving the stale cache.
    loaded_path: Option<PathBuf>,
}

fn store_path() -> PathBuf {
    if let Ok(raw) = std::env::var(EXCLUSIONS_PATH_ENV) {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    crate::paths::temp_root_dir().join("model-exclusions.json")
}

static STORE: Mutex<Store> = Mutex::new(Store {
    exclusions: Vec::new(),
    loaded: false,
    default_ttl_ms: DEFAULT_MODEL_EXCLUSION_TTL_MS,
    loaded_path: None,
});

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn dedup_key(entry: &ModelExclusion) -> (String, String) {
    (
        entry.provider.clone().unwrap_or_default(),
        entry.model_id.clone().unwrap_or_default(),
    )
}

fn deduplicate(items: Vec<ModelExclusion>) -> Vec<ModelExclusion> {
    let mut map: BTreeMap<(String, String), ModelExclusion> = BTreeMap::new();
    for item in items {
        let key = dedup_key(&item);
        match map.get(&key) {
            Some(existing) if existing.recorded_at >= item.recorded_at => {}
            _ => {
                map.insert(key, item);
            }
        }
    }
    map.into_values().collect()
}

/// `flushPersist`: atomic-ish write (tmp + rename) of the version-1 store.
fn flush_persist(exclusions: &[ModelExclusion]) {
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entries: Vec<Value> = exclusions
        .iter()
        .map(|entry| {
            let mut object = serde_json::Map::new();
            if let Some(model_id) = &entry.model_id {
                object.insert("modelId".into(), json!(model_id));
            }
            if let Some(provider) = &entry.provider {
                object.insert("provider".into(), json!(provider));
            }
            object.insert("reason".into(), json!(entry.reason));
            object.insert("recordedAt".into(), json!(entry.recorded_at));
            object.insert("expiresAt".into(), json!(entry.expires_at));
            Value::Object(object)
        })
        .collect();
    let body = json!({ "version": 1, "exclusions": entries });
    let Ok(raw) = serde_json::to_string_pretty(&body) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, raw).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn read_persisted(now: u64) -> Vec<ModelExclusion> {
    let Ok(raw) = std::fs::read_to_string(store_path()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    if value["version"] != json!(1) {
        return Vec::new();
    }
    let Some(entries) = value["exclusions"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let model_id = entry["modelId"].as_str().map(str::to_string);
        let provider = entry["provider"].as_str().map(str::to_string);
        if model_id.is_none() && provider.is_none() {
            continue;
        }
        let (Some(recorded_at), Some(expires_at)) =
            (entry["recordedAt"].as_u64(), entry["expiresAt"].as_u64())
        else {
            continue;
        };
        if expires_at <= now {
            continue;
        }
        out.push(ModelExclusion {
            model_id,
            provider,
            reason: entry["reason"]
                .as_str()
                .unwrap_or("runtime-failure")
                .to_string(),
            recorded_at,
            expires_at,
        });
    }
    out
}

/// Auth-flavored exclusions are dropped when the auth store changed after
/// they were recorded (`invalidateAuthExclusions`; rpi's auth store lives in
/// the host agent dir — absence means "cannot disprove", entries stay).
const AUTH_FAILURE_MARKERS: [&str; 6] = [
    "auth",
    "unauthori",
    "forbidden",
    "api key",
    "token expired",
    "invalid key",
];

fn is_auth_exclusion(entry: &ModelExclusion) -> bool {
    let reason = entry.reason.to_lowercase();
    AUTH_FAILURE_MARKERS
        .iter()
        .any(|marker| reason.contains(marker))
}

fn auth_store_mtime() -> Option<u64> {
    let path = crate::paths::get_agent_dir().join("auth.json");
    let metadata = std::fs::metadata(path).ok()?;
    use std::os::unix::fs::MetadataExt;
    Some(metadata.mtime() as u64)
}

fn ensure_loaded(store: &mut Store) {
    let current_path = store_path();
    if store.loaded && store.loaded_path.as_deref() == Some(current_path.as_path()) {
        return;
    }
    store.loaded = true;
    store.loaded_path = Some(current_path);
    let now = now_millis();
    store.exclusions = read_persisted(now);
    if let Some(mtime) = auth_store_mtime() {
        let before = store.exclusions.len();
        store
            .exclusions
            .retain(|entry| !is_auth_exclusion(entry) || mtime <= entry.recorded_at / 1000);
        if store.exclusions.len() != before {
            flush_persist(&store.exclusions);
        }
    }
}

/// `setDefaultTTL` (config `modelExclusions.defaultTtlMs`, #1439): overrides
/// the TTL applied to NEWLY recorded exclusions. Values beyond the ceiling
/// are clamped to it (the config layer already rejects them; this stays
/// defensive for programmatic callers).
pub fn set_default_ttl(ms: u64) {
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    store.default_ttl_ms = ms.clamp(1, MAX_MODEL_EXCLUSION_TTL_MS);
}

/// `recordModelFailure`: record a model (or provider-wide) failure with the
/// effective TTL; dedupe + cap + persist immediately.
pub fn record_model_failure(
    model_id: Option<String>,
    provider: Option<String>,
    reason: &str,
    ttl_ms: Option<u64>,
) {
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut store);
    let now = now_millis();
    let ttl = ttl_ms
        .unwrap_or(store.default_ttl_ms)
        .clamp(1, MAX_MODEL_EXCLUSION_TTL_MS);
    let entry = ModelExclusion {
        model_id,
        provider,
        reason: reason.to_string(),
        recorded_at: now,
        expires_at: now.saturating_add(ttl),
    };
    store.exclusions.insert(0, entry);
    store.exclusions = deduplicate(std::mem::take(&mut store.exclusions));
    if store.exclusions.len() > MAX_ENTRIES {
        let overflow = store.exclusions.len() - MAX_ENTRIES;
        store.exclusions.drain(..overflow);
    }
    flush_persist(&store.exclusions);
}

/// `entryMatches`: a model-specific exclusion matches that exact modelId
/// (providers must agree when both carry one); a provider-wide exclusion
/// matches every model of that provider.
fn entry_matches(entry: &ModelExclusion, model_id: &str, provider: Option<&str>, now: u64) -> bool {
    if entry.expires_at <= now {
        return false;
    }
    match &entry.model_id {
        Some(entry_model) => {
            if entry_model != model_id {
                return false;
            }
            match (&entry.provider, provider) {
                (Some(entry_provider), Some(candidate)) => entry_provider == candidate,
                _ => true,
            }
        }
        None => entry.provider.as_deref() == Some(provider.unwrap_or("")),
    }
}

/// `findModelExclusion`: the active exclusion for a full `provider/id`, if
/// any (hard-fail diagnostics).
pub fn find_model_exclusion(full_id: &str) -> Option<ModelExclusion> {
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut store);
    let store = &*store;
    let now = now_millis();
    let (provider, model_id) = split_model_key(full_id);
    store
        .exclusions
        .iter()
        .find(|entry| entry_matches(entry, model_id, provider, now))
        .cloned()
}

/// `parseModelKey`: `provider/id` → (`Some(provider)`, `id`); a bare id has
/// no provider.
fn split_model_key(model: &str) -> (Option<&str>, &str) {
    match model.split_once('/') {
        Some((provider, id)) => (Some(provider), id),
        None => (None, model),
    }
}

/// `filterFallbackCandidates`: drop excluded candidates (dedup along the
/// way); `on_excluded` observes each dropped candidate for diagnostics.
type ExcludedSink<'a> = Option<&'a mut dyn FnMut(&str, &ModelExclusion)>;

pub fn filter_fallback_candidates(
    candidates: Vec<String>,
    mut on_excluded: ExcludedSink<'_>,
) -> Vec<String> {
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut store);
    let store = &*store;
    let now = now_millis();
    let mut seen = std::collections::BTreeSet::new();
    let mut filtered = Vec::new();
    for candidate in candidates {
        if candidate.is_empty() || !seen.insert(candidate.clone()) {
            continue;
        }
        let (provider, model_id) = split_model_key(&candidate);
        let excluded = store
            .exclusions
            .iter()
            .find(|entry| entry_matches(entry, model_id, provider, now))
            .cloned();
        if let Some(exclusion) = excluded {
            if let Some(sink) = on_excluded.as_deref_mut() {
                sink(&candidate, &exclusion);
            }
            continue;
        }
        filtered.push(candidate);
    }
    filtered
}

/// `recordRetryableModelFailure` (model-fallback.ts:614-620): record only
/// retryable, non-overflow, non-request-shape failures.
pub fn record_retryable_model_failure(model: Option<&str>, error: Option<&str>) {
    let (Some(model), Some(error)) = (model, error) else {
        return;
    };
    if !super::model::is_retryable_model_failure(Some(error)) {
        return;
    }
    if super::model::is_context_overflow(Some(error)) {
        return;
    }
    // Request-shape failures (`bad request` / `invalid argument` /
    // `invalid_request_error`) say nothing about model health.
    let lowered = error.to_lowercase();
    if lowered.contains("bad request")
        || lowered.contains("bad_request")
        || lowered.contains("invalid argument")
        || lowered.contains("invalid_request_error")
    {
        return;
    }
    let (provider, model_id) = split_model_key(model);
    record_model_failure(
        Some(model_id.to_string()),
        provider.map(str::to_string),
        error,
        None,
    );
}

/// Test seam: reset the in-memory store and drop the file.
#[cfg(test)]
#[doc(hidden)]
pub fn reset_for_test() {
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    store.exclusions.clear();
    store.loaded = false;
    store.default_ttl_ms = DEFAULT_MODEL_EXCLUSION_TTL_MS;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EXCLUSIONS_PATH_ENV is process-global — the tests serialize through
    /// one lock and one scratch file.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scratch_path(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("rpi-model-excl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn record_filter_and_ttl_expiry_roundtrip() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = scratch_path("roundtrip");
        std::env::set_var(EXCLUSIONS_PATH_ENV, &path);
        reset_for_test();
        // Record a model-specific exclusion with a short TTL.
        record_model_failure(
            Some("gpt".to_string()),
            Some("openai".to_string()),
            "503 upstream",
            Some(60),
        );
        // Same model under another provider is NOT excluded (providers must
        // agree, upstream `entryMatches`).
        let mut seen = Vec::new();
        let filtered = filter_fallback_candidates(
            vec!["openai/gpt".into()],
            Some(&mut |c, _| seen.push(c.to_string())),
        );
        assert!(filtered.is_empty(), "{filtered:?}");
        let filtered =
            filter_fallback_candidates(vec!["github-copilot/gpt".into()], Some(&mut |_, _| {}));
        assert_eq!(filtered, vec!["github-copilot/gpt".to_string()]);
        // find_model_exclusion carries the reason for hard-fail diagnostics.
        let exclusion = find_model_exclusion("openai/gpt").unwrap();
        assert_eq!(exclusion.reason, "503 upstream");
        // TTL expiry drops the entry (expires_at <= now).
        let expired = ModelExclusion {
            model_id: Some("gpt".to_string()),
            provider: Some("openai".to_string()),
            reason: "stale".to_string(),
            recorded_at: 0,
            expires_at: 1,
        };
        assert!(!entry_matches(&expired, "gpt", Some("openai"), 1_000));
        // Provider-wide exclusions match every model of that provider.
        let provider_wide = ModelExclusion {
            model_id: None,
            provider: Some("openai".to_string()),
            reason: "quota".to_string(),
            recorded_at: 0,
            expires_at: u64::MAX,
        };
        assert!(entry_matches(
            &provider_wide,
            "anything",
            Some("openai"),
            100
        ));
        assert!(!entry_matches(
            &provider_wide,
            "thing",
            Some("anthropic"),
            100
        ));
        reset_for_test();
        let _ = std::fs::remove_file(&path);
        std::env::remove_var(EXCLUSIONS_PATH_ENV);
    }

    /// `sanitizeModelExclusionDiagnostic` (model-fallback.ts:296-301): control
    /// characters collapse, secrets redact, 240 cap, empty falls back.
    #[test]
    fn sanitize_diagnostic_redacts_and_caps() {
        // Control characters (incl. U+2028/U+2029) collapse to spaces.
        let cleaned = sanitize_diagnostic("boom\u{1}[2J done\u{2028}next", "fallback");
        assert!(cleaned.contains("boom"), "{cleaned}");
        assert!(!cleaned.contains('\u{1}'), "{cleaned}");
        assert!(!cleaned.contains('\u{2028}'), "{cleaned}");
        // Secret shapes redact (upstream SECRET_VALUE via redactSecretValues).
        let redacted = sanitize_diagnostic(
            "401 unauthorized: Bearer abc.def.ghi and sk-proj-0123456789abcdef",
            "fallback",
        );
        assert!(redacted.contains("[redacted]"), "{redacted}");
        assert!(!redacted.contains("abc.def.ghi"), "{redacted}");
        assert!(!redacted.contains("sk-proj-0123456789abcdef"), "{redacted}");
        // 240-char cap.
        let long = "x".repeat(600);
        assert_eq!(sanitize_diagnostic(&long, "f").chars().count(), 240);
        // Empty/whitespace-only falls back to the provided fallback.
        assert_eq!(
            sanitize_diagnostic("", "runtime-failure"),
            "runtime-failure"
        );
        assert_eq!(
            sanitize_diagnostic(" \u{1}\u{7f} ", "runtime-failure"),
            "runtime-failure"
        );
    }

    #[test]
    fn record_retryable_gates_on_error_shape() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(EXCLUSIONS_PATH_ENV, scratch_path("gating"));
        reset_for_test();
        // Distinct model ids per phase: reset_for_test clears memory but the
        // on-disk store survives (entries reload), so each assertion must
        // target a model no earlier phase recorded.
        record_retryable_model_failure(Some("openai/netfail"), Some("upstream 503 error"));
        assert!(find_model_exclusion("openai/netfail").is_some());
        // Context overflow never records (terminal, not model health).
        record_retryable_model_failure(Some("openai/overflow"), Some("context length exceeded"));
        assert!(find_model_exclusion("openai/overflow").is_none());
        // Request-shape failures never record.
        record_retryable_model_failure(
            Some("openai/badreq"),
            Some("400 bad request invalid_request_error"),
        );
        assert!(find_model_exclusion("openai/badreq").is_none());
        reset_for_test();
        std::env::remove_var(EXCLUSIONS_PATH_ENV);
    }

    #[test]
    fn store_persists_version_one_and_reloads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = scratch_path("persist");
        std::env::set_var(EXCLUSIONS_PATH_ENV, &path);
        reset_for_test();
        record_model_failure(
            Some("m".to_string()),
            Some("p".to_string()),
            "boom",
            Some(3_600_000),
        );
        // A fresh store (new process stand-in) reloads from disk.
        reset_for_test();
        assert!(
            find_model_exclusion("p/m").is_some(),
            "persisted entry reloads"
        );
        reset_for_test();
        let _ = std::fs::remove_file(&path);
        std::env::remove_var(EXCLUSIONS_PATH_ENV);
    }
}
