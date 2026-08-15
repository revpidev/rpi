//! Settings normalization — 1:1 port of upstream
//! `packages/pi-smart-fetch/src/settings.ts` @ b0111612 (FR-P1-5 surface;
//! TE06 ships the pure resolution + fixtures, TE07 wires the per-execute
//! file reads from `~/.rpi/agent/settings.json` + `.rpi/settings.json`).

use serde_json::Value;

use crate::constants::DEFAULT_TEMP_DIR_NAME;
use crate::types::{FetchToolConfig, IncludeReplies};

/// `VALID_OS_VALUES` (settings.ts:11-17).
const VALID_OS_VALUES: [&str; 5] = ["windows", "macos", "linux", "android", "ios"];

/// `ResolvedPiSmartFetchSettings` (settings.ts:31-33).
#[derive(Debug, Clone, Default)]
pub struct ResolvedSettings {
    pub verbose_by_default: bool,
    pub config: FetchToolConfig,
}

/// `readBoolean` (settings.ts:35-46): first key of the alias chain that holds
/// a real boolean wins.
fn read_boolean(source: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| source.get(key).and_then(Value::as_bool))
}

/// `readPositiveNumber` (settings.ts:48-60): finite positive numbers only.
fn read_positive_number(source: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        source
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
    })
}

/// `readNonEmptyString` (settings.ts:62-74).
fn read_non_empty_string(source: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        source
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    })
}

/// `readOs` (settings.ts:76-91).
fn read_os(source: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        source
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| VALID_OS_VALUES.contains(&FingerprintOs(value).as_str()))
            .map(str::to_string)
    })
}

/// `readIncludeReplies` (settings.ts:93-105): boolean or "extractors".
fn read_include_replies(source: &Value, keys: &[&str]) -> Option<IncludeReplies> {
    keys.iter().find_map(|key| match source.get(key) {
        Some(Value::Bool(true)) => Some(IncludeReplies::All),
        Some(Value::Bool(false)) => Some(IncludeReplies::None),
        Some(Value::String(s)) if s == "extractors" => Some(IncludeReplies::Extractors),
        _ => None,
    })
}

/// Normalized `PiSmartFetchSettings` — all reads resolved through the
/// `smartFetch*` → `webFetch*` alias chains (settings.ts:107-143).
#[derive(Debug, Clone, Default)]
pub struct NormalizedSettings {
    pub verbose_by_default: Option<bool>,
    pub default_max_chars: Option<u64>,
    pub default_timeout_ms: Option<u64>,
    pub default_browser: Option<String>,
    pub default_os: Option<String>,
    pub default_remove_images: Option<bool>,
    pub default_include_replies: Option<IncludeReplies>,
    pub default_batch_concurrency: Option<f64>,
    pub temp_dir: Option<String>,
}

/// `normalizePiSmartFetchSettings` (settings.ts:107-143).
pub fn normalize_settings(input: &Value) -> NormalizedSettings {
    let source = match input {
        Value::Object(map) => Value::Object(map.clone()),
        _ => Value::Object(Default::default()),
    };
    let positive_u64 =
        |keys: &[&str]| read_positive_number(&source, keys).map(|value| value as u64);
    NormalizedSettings {
        verbose_by_default: read_boolean(
            &source,
            &["smartFetchVerboseByDefault", "webFetchVerboseByDefault"],
        ),
        default_max_chars: positive_u64(&["smartFetchDefaultMaxChars", "webFetchDefaultMaxChars"]),
        default_timeout_ms: positive_u64(&["smartFetchDefaultTimeoutMs"]),
        default_browser: read_non_empty_string(&source, &["smartFetchDefaultBrowser"]),
        default_os: read_os(&source, &["smartFetchDefaultOs"]),
        default_remove_images: read_boolean(&source, &["smartFetchDefaultRemoveImages"]),
        default_include_replies: read_include_replies(
            &source,
            &["smartFetchDefaultIncludeReplies"],
        ),
        default_batch_concurrency: positive_u64(&[
            "smartFetchDefaultBatchConcurrency",
            "webFetchDefaultBatchConcurrency",
        ])
        .map(|value| value as f64),
        temp_dir: read_non_empty_string(&source, &["smartFetchTempDir", "webFetchTempDir"]),
    }
}

/// `resolvePiSmartFetchSettings` (settings.ts:145-178): project overrides
/// global per key; the temp dir default is [VARIANT] `smart-fetch-rpi`
/// (upstream `smart-fetch-pi`, requirements §3).
pub fn resolve_settings(global: &Value, project: &Value) -> ResolvedSettings {
    let global = normalize_settings(global);
    let project = normalize_settings(project);

    let verbose_by_default = project
        .verbose_by_default
        .or(global.verbose_by_default)
        .unwrap_or(false);

    ResolvedSettings {
        verbose_by_default,
        config: FetchToolConfig {
            max_chars: project.default_max_chars.or(global.default_max_chars),
            timeout_ms: project.default_timeout_ms.or(global.default_timeout_ms),
            browser: project.default_browser.or(global.default_browser),
            os: project.default_os.or(global.default_os),
            remove_images: project
                .default_remove_images
                .or(global.default_remove_images),
            include_replies: project
                .default_include_replies
                .or(global.default_include_replies),
            batch_concurrency: project
                .default_batch_concurrency
                .or(global.default_batch_concurrency),
            temp_dir: project.temp_dir.or(global.temp_dir).or_else(|| {
                Some(
                    std::env::temp_dir()
                        .join(DEFAULT_TEMP_DIR_NAME)
                        .to_string_lossy()
                        .to_string(),
                )
            }),
        },
    }
}

/// Extension for the OS validity helper above (`&str` newtype keeps the
/// closure readable; mirrors upstream's Set.has).
struct FingerprintOs<'a>(&'a str);

impl FingerprintOs<'_> {
    fn as_str(&self) -> &'static str {
        match self.0 {
            "windows" => "windows",
            "macos" => "macos",
            "linux" => "linux",
            "android" => "android",
            "ios" => "ios",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn alias_chains_and_type_guards() {
        let source = json!({
            "webFetchVerboseByDefault": true,             // legacy alias
            "smartFetchDefaultMaxChars": "not a number",   // wrong type → ignored
            "webFetchDefaultMaxChars": 1234,               // legacy alias number
            "smartFetchDefaultOs": "Haiku",                // invalid OS → ignored
            "smartFetchDefaultIncludeReplies": false,
            "smartFetchTempDir": "   ",                    // blank → ignored
            "webFetchTempDir": "/tmp/custom",
        });
        let normalized = normalize_settings(&source);
        assert_eq!(normalized.verbose_by_default, Some(true));
        assert_eq!(normalized.default_max_chars, Some(1234));
        assert_eq!(normalized.default_os, None);
        assert_eq!(
            normalized.default_include_replies,
            Some(IncludeReplies::None)
        );
        assert_eq!(normalized.temp_dir.as_deref(), Some("/tmp/custom"));
    }

    #[test]
    fn project_overrides_global_per_key() {
        let global = json!({
            "smartFetchDefaultMaxChars": 1000,
            "smartFetchDefaultBrowser": "chrome_145",
        });
        let project = json!({
            "smartFetchDefaultMaxChars": 2000,
        });
        let resolved = resolve_settings(&global, &project);
        assert_eq!(resolved.config.max_chars, Some(2000));
        assert_eq!(resolved.config.browser.as_deref(), Some("chrome_145"));
        // [VARIANT] temp dir default carries the rpi name.
        assert!(resolved
            .config
            .temp_dir
            .as_deref()
            .is_some_and(|dir| dir.ends_with(DEFAULT_TEMP_DIR_NAME)));
    }
}
