//! XDG config reading for `rpiv-todo` (maxWidgetLines / collapseKey /
//! guidance overrides).
//!
//! Port of upstream `packages/rpiv-todo/config.ts` @ `0fdf4f8` plus the
//! `@juicesharp/rpiv-config` loader it calls
//! (`loadJsonConfigWithLegacyFallback` / `validateGuidanceFields`).
//!
//! Path policy (R-T6 [PARITY/VARIANT], deviation TE-D43): the upstream
//! path `~/.config/rpiv-todo/config.json` is kept verbatim — brand
//! independent, deliberately NOT mapped to `~/.rpi` (TE-D39 precedent).
//! Lookup order (read-only, never created):
//!
//! 1. `$XDG_CONFIG_HOME/rpiv-todo/config.json` — must be set, non-empty
//!    after trim and absolute (`~`/`~/…` expands first; relative falls
//!    through);
//! 2. else the legacy `~/.config/rpiv-todo/config.json` (intentionally
//!    ignores `XDG_CONFIG_HOME`).
//!
//! A present XDG file wins even when malformed (corruption is surfaced,
//! not masked). Malformed JSON warns (via `tracing`, the rpi diagnostic
//! channel — upstream `console.warn`) and yields defaults; valid
//! non-object JSON and per-key type errors fall back silently.
//!
//! Upstream reads the config **fresh from disk on every call**
//! (`getMaxWidgetLines` / `resolveCollapseKey` are per-render; the
//! shortcut registration and guidance resolution read once at factory
//! scope — a rebind or guidance swap needs a restart, requirements §7).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Config directory name (upstream package directory).
pub const CONFIG_DIR_NAME: &str = "rpiv-todo";
/// Config file name.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Default content-row budget when the config is missing/invalid
/// (upstream `DEFAULT_MAX_WIDGET_LINES` — the prior hardcoded value,
/// preserved as the fallback).
pub const DEFAULT_MAX_WIDGET_LINES: usize = 12;
/// Minimum accepted `maxWidgetLines` value; below it the DEFAULT applies
/// (upstream floor check — values 2/1/0/-5 fall back to 12, they are NOT
/// clamped to 3; task-file §2 FR-E "钳制 3" wording corrected in §7.3).
pub const MIN_WIDGET_LINES: i64 = 3;

/// Default collapse/expand key (upstream `DEFAULT_COLLAPSE_KEY`).
pub const DEFAULT_COLLAPSE_KEY: &str = "ctrl+shift+t";
/// Sentinel value that disables the collapse shortcut entirely
/// (upstream `COLLAPSE_KEY_OFF`).
pub const COLLAPSE_KEY_OFF: &str = "off";

// ---------------------------------------------------------------------------
// Guidance fields (rpiv-config `validateGuidanceFields`)
// ---------------------------------------------------------------------------

/// Guidance overrides for the registered tool (upstream `GuidanceFields`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuidanceFields {
    /// Replaces the default tool description when non-empty.
    pub description: Option<String>,
    /// Replaces the default prompt snippet when non-empty.
    pub prompt_snippet: Option<String>,
    /// Replaces the default prompt guidelines (non-empty array of
    /// non-empty strings) when present.
    pub prompt_guidelines: Option<Vec<String>>,
}

/// Validate and extract guidance fields (upstream `validateGuidanceFields`
/// — clean object with only valid, non-empty entries).
pub fn validate_guidance_fields(fields: Option<&Value>) -> GuidanceFields {
    let Some(fields) = fields.and_then(Value::as_object) else {
        return GuidanceFields::default();
    };
    let non_empty_string = |key: &str| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let guidelines = fields
        .get("promptGuidelines")
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .and_then(|values| {
            let strings: Vec<String> = values
                .iter()
                .map(|value| value.as_str().unwrap_or(""))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect();
            (strings.len() == values.len()).then_some(strings)
        });
    GuidanceFields {
        description: non_empty_string("description"),
        prompt_snippet: non_empty_string("promptSnippet"),
        prompt_guidelines: guidelines,
    }
}

// ---------------------------------------------------------------------------
// Path resolution (rpiv-config `configPath` / `legacyConfigPath`)
// ---------------------------------------------------------------------------

/// Home directory (`$HOME` on Unix, `%USERPROFILE%` on Windows — the
/// mcp-adapter `home_dir` convention).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Expand a leading `~` (upstream `expandTilde`): only bare `~` and `~/…`
/// expand; `~user` is returned unchanged (XDG defines no `~user` form).
fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Default config directory `~/.config` (upstream `defaultConfigDir`).
pub fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".config")
}

/// Resolve the config directory honoring `XDG_CONFIG_HOME` (upstream
/// `resolveConfigDir`): unset/empty/whitespace-only or relative →
/// `~/.config`.
pub fn resolve_config_dir(xdg_config_home: Option<&str>, home: &Path) -> PathBuf {
    let Some(raw) = xdg_config_home else {
        return default_config_dir(home);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_config_dir(home);
    }
    let expanded = expand_tilde(trimmed, home);
    if expanded.is_absolute() {
        expanded
    } else {
        default_config_dir(home)
    }
}

/// XDG-resolved config path (upstream `configPath`).
pub fn config_path(xdg_config_home: Option<&str>, home: &Path) -> PathBuf {
    resolve_config_dir(xdg_config_home, home)
        .join(CONFIG_DIR_NAME)
        .join(CONFIG_FILE_NAME)
}

/// Always-legacy config path under `~/.config` (upstream
/// `legacyConfigPath` — ignores `XDG_CONFIG_HOME` by design).
pub fn legacy_config_path(home: &Path) -> PathBuf {
    default_config_dir(home)
        .join(CONFIG_DIR_NAME)
        .join(CONFIG_FILE_NAME)
}

/// Parse a config file (upstream `loadJsonConfig`): missing/malformed/
/// non-plain-object all yield an empty object; malformed JSON warns.
fn load_json_config(path: &Path) -> Value {
    if !path.exists() {
        return Value::Object(serde_json::Map::new());
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "rpiv-todo: config read failed, using defaults");
            return Value::Object(serde_json::Map::new());
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) if value.is_object() => value,
        Ok(_) => Value::Object(serde_json::Map::new()),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "rpiv-todo: invalid JSON, using default ({{}})");
            Value::Object(serde_json::Map::new())
        }
    }
}

/// Load the raw config object (upstream `loadJsonConfigWithLegacyFallback`):
/// XDG file present (valid or malformed) wins; XDG missing → legacy path;
/// both missing → `{}`.
pub fn load_raw_config(xdg_config_home: Option<&str>, home: &Path) -> Value {
    let xdg_path = config_path(xdg_config_home, home);
    if xdg_path.exists() {
        return load_json_config(&xdg_path);
    }
    load_json_config(&legacy_config_path(home))
}

/// Load the config for explicit inputs (test seam; production
/// [`load_config`] reads the process environment). Fresh read, no
/// caching — mirrors upstream's per-call disk read.
pub fn load_config_from(xdg_config_home: Option<&str>, home: &Path) -> Value {
    load_raw_config(xdg_config_home, home)
}

/// Load the config for the current process (upstream `loadConfig`).
pub fn load_config() -> Value {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = home_dir().unwrap_or_else(|| PathBuf::from("/"));
    load_config_from(xdg.as_deref(), &home)
}

// ---------------------------------------------------------------------------
// maxWidgetLines (upstream `getMaxWidgetLines`)
// ---------------------------------------------------------------------------

/// Content-row budget for the overlay, read fresh on every call
/// (per-render — no restart). A non-number or a value below the floor of
/// 3 falls back to the default; no ceiling.
pub fn get_max_widget_lines() -> usize {
    let lines = load_config().get("maxWidgetLines").cloned();
    match lines {
        Some(Value::Number(number))
            if number
                .as_i64()
                .is_some_and(|value| value >= MIN_WIDGET_LINES) =>
        {
            number.as_i64().unwrap_or(DEFAULT_MAX_WIDGET_LINES as i64) as usize
        }
        _ => DEFAULT_MAX_WIDGET_LINES,
    }
}

// ---------------------------------------------------------------------------
// collapseKey (upstream `isValidCollapseKeySpec` / `resolveCollapseKey`)
// ---------------------------------------------------------------------------

/// Named keys accepted by pi-tui's `matchesKey` (upstream `SPECIAL_KEYS` —
/// keys.js switches on the parsed base key; `parseKeyId` lowercases the id
/// before matching, so lowercase spellings are canonical).
const SPECIAL_KEYS: [&str; 27] = [
    "escape",
    "esc",
    "enter",
    "return",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "clear",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
];
const SPECIAL_KEYS_TAIL: [&str; 3] = ["f10", "f11", "f12"];
const MODIFIERS: [&str; 4] = ["ctrl", "shift", "alt", "super"];

fn is_special_key(key: &str) -> bool {
    SPECIAL_KEYS.contains(&key) || SPECIAL_KEYS_TAIL.contains(&key)
}

/// Single-printable-character base key (upstream regex
/// `/[a-z0-9_\-!@#$%^&*()|~`'":;,./<>?[\]{}=\\]/`).
fn is_printable_base(key: char) -> bool {
    key.is_ascii_lowercase()
        || key.is_ascii_digit()
        || "_-!@#$%^&*()|~`'\":;,./<>?[]{}=\\".contains(key)
}

/// Validate a collapse-key spec against pi-tui's `KeyId` grammar
/// (upstream `isValidCollapseKeySpec`, itself a verbatim port from
/// rpiv-ask-user-question): zero or more distinct modifiers, then a base
/// key that is a single printable character or a named special key.
///
/// A loose check is not enough — pi-tui's `parseKeyId` takes the LAST
/// `+`-part as the key and ignores unknown parts, so a typo like `ctr+]`
/// would silently match every bare `]` keypress.
pub fn is_valid_collapse_key_spec(spec: &str) -> bool {
    if spec.is_empty() || spec.starts_with('+') || spec.ends_with('+') || spec.contains("++") {
        return false;
    }
    let parts: Vec<&str> = spec.split('+').collect();
    let base = *parts.last().unwrap_or(&"");
    let modifiers = &parts[..parts.len() - 1];
    let unique: BTreeSet<&&str> = modifiers.iter().collect();
    if unique.len() != modifiers.len() {
        return false;
    }
    if !modifiers
        .iter()
        .all(|modifier| MODIFIERS.contains(modifier))
    {
        return false;
    }
    let mut chars = base.chars();
    match (chars.next(), chars.next()) {
        (Some(single), None) => is_printable_base(single),
        _ => is_special_key(base),
    }
}

/// Resolve the collapse/expand key from config, read fresh on every call
/// (per-render / per-registration — mirrors `getMaxWidgetLines`).
/// Returns [`DEFAULT_COLLAPSE_KEY`] when the field is missing/non-string/
/// empty/blank/invalid, [`COLLAPSE_KEY_OFF`] when set to the sentinel, or
/// the lowercased validated spec.
pub fn resolve_collapse_key() -> String {
    resolve_collapse_key_from(&load_config())
}

/// Test seam over an explicit raw config object.
pub fn resolve_collapse_key_from(config: &Value) -> String {
    let raw = config
        .get("collapseKey")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_lowercase());
    match raw {
        None => DEFAULT_COLLAPSE_KEY.to_owned(),
        Some(raw) if raw.is_empty() => DEFAULT_COLLAPSE_KEY.to_owned(),
        Some(raw) if raw == COLLAPSE_KEY_OFF => COLLAPSE_KEY_OFF.to_owned(),
        Some(raw) if is_valid_collapse_key_spec(&raw) => raw,
        Some(_) => DEFAULT_COLLAPSE_KEY.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    //! Port of upstream `config.test.ts` @ `0fdf4f8`. The upstream suite
    //! writes real files under `$HOME/.config`; the rpi seam injects
    //! `xdg_config_home` + `home` (temp dirs) instead — no process-global
    //! filesystem state (the ask-user-question config.rs precedent).

    use super::*;
    use serde_json::json;

    /// One temp HOME with an optional XDG value; returns (home, xdg).
    /// `write` populates the config file under the resolved location.
    fn scratch(write: Option<(Option<&str>, &Value)>) -> (PathBuf, Option<String>) {
        let home = std::env::temp_dir().join(format!(
            "rpiv-todo-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let xdg = home.join("xdg");
        std::fs::create_dir_all(&xdg).expect("scratch dirs");
        let xdg_value = Some(xdg.to_str().expect("utf8 path").to_owned());
        if let Some((use_xdg, value)) = write {
            let path = if use_xdg.is_some() {
                config_path(xdg_value.as_deref(), &home)
            } else {
                legacy_config_path(&home)
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("config parent");
            }
            std::fs::write(&path, serde_json::to_string_pretty(value).expect("json"))
                .expect("write config");
        }
        (home, xdg_value)
    }

    // ------------------------------------------------------------------
    // getMaxWidgetLines (per-value matrix, upstream suite semantics)
    // ------------------------------------------------------------------

    #[test]
    fn max_widget_lines_returns_the_default_when_no_config_is_present() {
        let (home, xdg) = scratch(None);
        assert_eq!(
            get_max_widget_lines_from(xdg.as_deref(), &home),
            DEFAULT_MAX_WIDGET_LINES
        );
        assert_eq!(DEFAULT_MAX_WIDGET_LINES, 12);
    }

    #[test]
    fn max_widget_lines_returns_the_default_for_non_number_values() {
        let (home, xdg) = scratch(Some((None, &json!({"maxWidgetLines": "twelve"}))));
        assert_eq!(
            get_max_widget_lines_from(xdg.as_deref(), &home),
            DEFAULT_MAX_WIDGET_LINES
        );
    }

    #[test]
    fn max_widget_lines_returns_the_default_for_values_below_the_floor() {
        for bad in [2, 1, 0, -5] {
            let (home, xdg) = scratch(Some((None, &json!({ "maxWidgetLines": bad }))));
            assert_eq!(
                get_max_widget_lines_from(xdg.as_deref(), &home),
                DEFAULT_MAX_WIDGET_LINES,
                "value {bad} must fall back to the default"
            );
        }
    }

    #[test]
    fn max_widget_lines_accepts_the_floor_and_above_with_no_ceiling() {
        for good in [3, 8, 50] {
            let (home, xdg) = scratch(Some((None, &json!({ "maxWidgetLines": good }))));
            assert_eq!(
                get_max_widget_lines_from(xdg.as_deref(), &home),
                good as usize
            );
        }
    }

    #[test]
    fn max_widget_lines_ignores_a_float_value() {
        // JSON 12.5 is a number but not an integer — the upstream typeof
        // check accepts it (JS numbers are all doubles) and the value
        // passes the >= 3 comparison; the render budget then receives a
        // fractional line count that JS truncates per comparison. rpi's
        // i64 projection treats non-integral numbers as invalid → default
        // (task-file §7.3 ruling 5: integral-domain normalization).
        let (home, xdg) = scratch(Some((None, &json!({ "maxWidgetLines": 12.5 }))));
        assert_eq!(
            get_max_widget_lines_from(xdg.as_deref(), &home),
            DEFAULT_MAX_WIDGET_LINES
        );
    }

    /// Explicit-input seam for the budget (production reads the process
    /// environment; tests inject home/xdg).
    fn get_max_widget_lines_from(xdg: Option<&str>, home: &Path) -> usize {
        let lines = load_config_from(xdg, home).get("maxWidgetLines").cloned();
        match lines {
            Some(Value::Number(number))
                if number
                    .as_i64()
                    .is_some_and(|value| value >= MIN_WIDGET_LINES) =>
            {
                number.as_i64().unwrap_or(DEFAULT_MAX_WIDGET_LINES as i64) as usize
            }
            _ => DEFAULT_MAX_WIDGET_LINES,
        }
    }

    // ------------------------------------------------------------------
    // loadConfig — collapseKey passthrough (validation at resolve)
    // ------------------------------------------------------------------

    #[test]
    fn load_config_surfaces_a_user_set_collapse_key_unchanged() {
        let (home, xdg) = scratch(Some((None, &json!({ "collapseKey": "alt+o" }))));
        let raw = load_config_from(xdg.as_deref(), &home);
        assert_eq!(raw.get("collapseKey"), Some(&json!("alt+o")));
    }

    #[test]
    fn load_config_passes_invalid_specs_through_verbatim() {
        let (home, xdg) = scratch(Some((None, &json!({ "collapseKey": "ctr+t" }))));
        let raw = load_config_from(xdg.as_deref(), &home);
        assert_eq!(raw.get("collapseKey"), Some(&json!("ctr+t")));
    }

    #[test]
    fn load_config_malformed_json_falls_back_to_an_empty_object() {
        let home =
            std::env::temp_dir().join(format!("rpiv-todo-config-bad-{}", std::process::id()));
        let path = legacy_config_path(&home);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, "{not json").expect("write");
        let raw = load_config_from(None, &home);
        assert!(raw.as_object().is_some_and(|map| map.is_empty()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn load_config_non_object_json_falls_back_to_an_empty_object() {
        let home =
            std::env::temp_dir().join(format!("rpiv-todo-config-arr-{}", std::process::id()));
        let path = legacy_config_path(&home);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, "[1,2]").expect("write");
        let raw = load_config_from(None, &home);
        assert!(raw.as_object().is_some_and(|map| map.is_empty()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn xdg_file_wins_over_legacy_even_when_malformed() {
        let home = std::env::temp_dir().join(format!(
            "rpiv-todo-config-xdg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let xdg_dir = home.join("xdg");
        std::fs::create_dir_all(xdg_dir.join(CONFIG_DIR_NAME)).expect("dirs");
        std::fs::create_dir_all(home.join(".config").join(CONFIG_DIR_NAME)).expect("dirs");
        std::fs::write(xdg_dir.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME), "{bad")
            .expect("write xdg");
        std::fs::write(
            home.join(".config")
                .join(CONFIG_DIR_NAME)
                .join(CONFIG_FILE_NAME),
            serde_json::to_string(&json!({"collapseKey": "legacy-key"})).expect("json"),
        )
        .expect("write legacy");
        let raw = load_config_from(Some(xdg_dir.to_str().expect("utf8")), &home);
        // Malformed XDG warns and yields {} — it does NOT fall back to the
        // legacy file (corruption is surfaced, not masked).
        assert!(raw.as_object().is_some_and(|map| map.is_empty()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_xdg_falls_back_to_the_legacy_path() {
        let home = std::env::temp_dir().join(format!(
            "rpiv-todo-config-legacy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = legacy_config_path(&home);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"collapseKey": "alt+o"})).expect("json"),
        )
        .expect("write");
        let raw = load_config_from(Some("/nonexistent-xdg"), &home);
        assert_eq!(raw.get("collapseKey"), Some(&json!("alt+o")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn relative_xdg_value_falls_back_to_the_default_config_dir() {
        let home = std::env::temp_dir().join(format!(
            "rpiv-todo-config-rel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = legacy_config_path(&home);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"maxWidgetLines": 7})).expect("json"),
        )
        .expect("write");
        let raw = load_config_from(Some("relative/path"), &home);
        assert_eq!(raw.get("maxWidgetLines"), Some(&json!(7)));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn tilde_xdg_value_expands_to_home() {
        let home = std::env::temp_dir().join(format!(
            "rpiv-todo-config-tilde-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let xdg_dir = home.join("xdg");
        std::fs::create_dir_all(xdg_dir.join(CONFIG_DIR_NAME)).expect("dirs");
        std::fs::write(
            xdg_dir.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME),
            serde_json::to_string(&json!({"maxWidgetLines": 5})).expect("json"),
        )
        .expect("write");
        let raw = load_config_from(Some("~/xdg"), &home);
        assert_eq!(raw.get("maxWidgetLines"), Some(&json!(5)));
        let _ = std::fs::remove_dir_all(&home);
    }

    // ------------------------------------------------------------------
    // resolveCollapseKey (upstream matrix)
    // ------------------------------------------------------------------

    #[test]
    fn resolve_collapse_key_defaults_without_a_field() {
        assert_eq!(resolve_collapse_key_from(&json!({})), DEFAULT_COLLAPSE_KEY);
        assert_eq!(DEFAULT_COLLAPSE_KEY, "ctrl+shift+t");
    }

    #[test]
    fn resolve_collapse_key_defaults_on_empty_or_blank() {
        assert_eq!(
            resolve_collapse_key_from(&json!({"collapseKey": ""})),
            DEFAULT_COLLAPSE_KEY
        );
        assert_eq!(
            resolve_collapse_key_from(&json!({"collapseKey": "   "})),
            DEFAULT_COLLAPSE_KEY
        );
    }

    #[test]
    fn resolve_collapse_key_returns_the_off_sentinel() {
        assert_eq!(
            resolve_collapse_key_from(&json!({"collapseKey": "off"})),
            COLLAPSE_KEY_OFF
        );
    }

    #[test]
    fn resolve_collapse_key_lowercases_a_valid_spec() {
        assert_eq!(
            resolve_collapse_key_from(&json!({"collapseKey": "Alt+O"})),
            "alt+o"
        );
    }

    #[test]
    fn resolve_collapse_key_defaults_on_an_invalid_spec() {
        assert_eq!(
            resolve_collapse_key_from(&json!({"collapseKey": "ctr+t"})),
            DEFAULT_COLLAPSE_KEY
        );
    }

    #[test]
    fn resolve_collapse_key_defaults_on_non_string_values() {
        for bad in [
            json!(123),
            json!(true),
            json!(["alt+o"]),
            json!({"key": "alt+o"}),
        ] {
            let config = json!({ "collapseKey": bad });
            assert_eq!(resolve_collapse_key_from(&config), DEFAULT_COLLAPSE_KEY);
        }
    }

    // ------------------------------------------------------------------
    // isValidCollapseKeySpec (upstream matrix)
    // ------------------------------------------------------------------

    #[test]
    fn key_spec_accepts_valid_specs() {
        assert!(is_valid_collapse_key_spec("ctrl+shift+t"));
        assert!(is_valid_collapse_key_spec("alt+o"));
        assert!(is_valid_collapse_key_spec("escape"));
        assert!(is_valid_collapse_key_spec("f5"));
        assert!(is_valid_collapse_key_spec("ctrl+]"));
    }

    #[test]
    fn key_spec_rejects_empty_and_plus_edge_cases() {
        for bad in ["", "+", "+t", "ctrl+", "ctrl++t"] {
            assert!(!is_valid_collapse_key_spec(bad), "{bad:?} must be invalid");
        }
    }

    #[test]
    fn key_spec_rejects_unknown_and_duplicate_modifiers() {
        assert!(!is_valid_collapse_key_spec("win+t"));
        assert!(!is_valid_collapse_key_spec("ctrl+ctrl+t"));
    }

    #[test]
    fn key_spec_rejects_multi_char_non_special_bases() {
        // 'ctr' is 3 chars and not a named special key — a typo for 'ctrl'.
        assert!(!is_valid_collapse_key_spec("ctr+t"));
    }

    // ------------------------------------------------------------------
    // validateGuidanceFields (guidance override surface; the tool-side
    // override tests live in tool.rs — upstream todo.guidance.test.ts)
    // ------------------------------------------------------------------

    #[test]
    fn guidance_validation_keeps_only_non_empty_valid_fields() {
        assert_eq!(
            validate_guidance_fields(Some(&json!({}))),
            GuidanceFields::default()
        );
        assert_eq!(validate_guidance_fields(None), GuidanceFields::default());
        assert_eq!(
            validate_guidance_fields(Some(&json!("nope"))),
            GuidanceFields::default()
        );
        let fields = validate_guidance_fields(Some(&json!({
            "promptSnippet": "Custom",
            "promptGuidelines": ["Rule one", "Rule two"],
            "description": "Custom description",
        })));
        assert_eq!(fields.prompt_snippet.as_deref(), Some("Custom"));
        assert_eq!(
            fields.prompt_guidelines,
            Some(vec!["Rule one".to_owned(), "Rule two".to_owned()])
        );
        assert_eq!(fields.description.as_deref(), Some("Custom description"));
    }

    #[test]
    fn guidance_validation_drops_empty_and_wrong_typed_entries() {
        // Empty promptSnippet → dropped.
        let fields = validate_guidance_fields(Some(&json!({"promptSnippet": ""})));
        assert_eq!(fields.prompt_snippet, None);
        // Wrong types → dropped.
        let fields = validate_guidance_fields(Some(&json!({
            "promptSnippet": 123,
            "promptGuidelines": "not-array",
        })));
        assert_eq!(fields.prompt_snippet, None);
        assert_eq!(fields.prompt_guidelines, None);
        // Array with an empty-string item → the whole field is dropped
        // (upstream `every` semantics).
        let fields = validate_guidance_fields(Some(&json!({
            "promptGuidelines": ["valid", ""],
        })));
        assert_eq!(fields.prompt_guidelines, None);
        // Empty array → dropped.
        let fields = validate_guidance_fields(Some(&json!({"promptGuidelines": []})));
        assert_eq!(fields.prompt_guidelines, None);
    }
}
