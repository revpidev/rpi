//! XDG config reading for `rpiv-ask-user-question`.
//!
//! Port of upstream `packages/rpiv-ask-user-question/config.ts` plus the
//! `@juicesharp/rpiv-config` loader it calls (`config.ts`/`loadJsonConfig`/
//! `loadJsonConfigWithLegacyFallback`/`validateGuidanceFields`) @ `338b264c`.
//!
//! Path policy (R-Q7.1 [VARIANT], deviation TE-D39): the upstream path
//! `~/.config/rpiv-ask-user-question/config.json` is kept verbatim — it is
//! brand-independent and deliberately NOT mapped to `~/.rpi`. Lookup order
//! (requirements 附录 D, read-only, never created):
//!
//! 1. `$XDG_CONFIG_HOME/rpiv-ask-user-question/config.json` — `XDG_CONFIG_HOME`
//!    must be set, non-empty after trim and absolute (`~`/`~/…` expands first;
//!    relative values fall through);
//! 2. else the legacy `~/.config/rpiv-ask-user-question/config.json`
//!    (intentionally ignores `XDG_CONFIG_HOME`);
//! 3. else all defaults.
//!
//! A present XDG file wins even when malformed (corruption is surfaced, not
//! masked). Malformed JSON warns (via `tracing`, the rpi diagnostic channel —
//! upstream uses `console.warn`) and yields defaults; valid non-object JSON
//! and per-key type errors fall back silently.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::tool::types::{MAX_QUESTIONS, MIN_OPTIONS};

/// Default collapse/expand key spec (`DEFAULT_COLLAPSE_KEY`).
pub const DEFAULT_COLLAPSE_KEY: &str = "ctrl+]";
/// Sentinel value disabling the collapse shortcut (`COLLAPSE_KEY_OFF`).
pub const COLLAPSE_KEY_OFF: &str = "off";
/// Config directory name (upstream package directory).
pub const CONFIG_DIR_NAME: &str = "rpiv-ask-user-question";
/// Config file name.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Guidance overrides for the registered tool (`GuidanceFields`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuidanceFields {
    /// Replaces the default tool description when non-empty.
    pub description: Option<String>,
    /// Replaces the default prompt snippet when non-empty.
    pub prompt_snippet: Option<String>,
    /// Replaces the default prompt guidelines when non-empty.
    pub prompt_guidelines: Option<Vec<String>>,
}

/// The plugin config surface (upstream `AskUserQuestionConfig`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskUserQuestionConfig {
    /// Validated guidance overrides.
    pub guidance: GuidanceFields,
    /// Raw `collapseKey` string when the JSON value was a string.
    pub collapse_key: Option<String>,
}

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

/// Expand a leading `~` (upstream `expandTildePath`): only bare `~` and
/// `~/…` expand; `~user` is returned unchanged (XDG defines no `~user` form).
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
/// `resolveConfigDir`): unset/empty/whitespace-only or relative → `~/.config`.
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

/// XDG-resolved config path (`configPath`).
pub fn config_path(xdg_config_home: Option<&str>, home: &Path) -> PathBuf {
    resolve_config_dir(xdg_config_home, home)
        .join(CONFIG_DIR_NAME)
        .join(CONFIG_FILE_NAME)
}

/// Always-legacy config path under `~/.config` (`legacyConfigPath`).
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
            tracing::warn!(path = %path.display(), %error, "rpiv-ask-user-question: config read failed, using defaults");
            return Value::Object(serde_json::Map::new());
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) if value.is_object() => value,
        Ok(_) => Value::Object(serde_json::Map::new()),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "rpiv-ask-user-question: invalid JSON, using defaults");
            Value::Object(serde_json::Map::new())
        }
    }
}

/// Load the raw config object (upstream `loadJsonConfigWithLegacyFallback`).
pub fn load_raw_config(xdg_config_home: Option<&str>, home: &Path) -> Value {
    let xdg_path = config_path(xdg_config_home, home);
    if xdg_path.exists() {
        return load_json_config(&xdg_path);
    }
    load_json_config(&legacy_config_path(home))
}

/// Validate and extract guidance fields (upstream `validateGuidanceFields`).
/// Returns a clean object with only valid, non-empty entries.
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

/// Named keys accepted by the key-id grammar (upstream `SPECIAL_KEYS`).
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

/// Mirror pi-tui's `KeyId` grammar strictly (upstream
/// `isValidCollapseKeySpec`): zero or more distinct modifiers, then a base key
/// that is a single printable character or a named special key.
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

/// Resolve the effective collapse key (upstream `resolveCollapseKey`):
/// trim + lowercase, empty/unset → default, `"off"` disables, invalid → default.
pub fn resolve_collapse_key(config: &AskUserQuestionConfig) -> String {
    let raw = config
        .collapse_key
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if raw.is_empty() {
        return DEFAULT_COLLAPSE_KEY.to_owned();
    }
    if raw == COLLAPSE_KEY_OFF {
        return COLLAPSE_KEY_OFF.to_owned();
    }
    if is_valid_collapse_key_spec(&raw) {
        raw
    } else {
        DEFAULT_COLLAPSE_KEY.to_owned()
    }
}

/// Pretty-print a resolved key spec for UI copy (upstream
/// `formatKeySpecForDisplay`); display-only, never fed back into matching.
pub fn format_key_spec_for_display(spec: &str) -> String {
    spec.split('+')
        .map(|part| match part {
            "pageup" => "PageUp".to_owned(),
            "pagedown" => "PageDown".to_owned(),
            _ => {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) if chars.clone().next().is_none() => {
                        first.to_uppercase().collect::<String>()
                    }
                    Some(first) => {
                        format!("{}{}", first.to_uppercase(), chars.as_str())
                    }
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<String>>()
        .join("+")
}

/// Load + validate the config from explicit inputs (test seam; production
/// [`load_config`] reads the process environment).
pub fn load_config_from(xdg_config_home: Option<&str>, home: &Path) -> AskUserQuestionConfig {
    let raw = load_raw_config(xdg_config_home, home);
    AskUserQuestionConfig {
        guidance: validate_guidance_fields(raw.get("guidance")),
        collapse_key: raw
            .get("collapseKey")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Load the config for the current process (`loadConfig`).
pub fn load_config() -> AskUserQuestionConfig {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = home_dir().unwrap_or_else(|| PathBuf::from("/"));
    load_config_from(xdg.as_deref(), &home)
}

/// Default tool description (`DEFAULT_TOOL_DESCRIPTION`), byte-equal to
/// upstream `ask-user-question.ts`.
pub const DEFAULT_TOOL_DESCRIPTION: &str = "Ask the user one or more structured questions during execution. Use when you need to:\n1. Gather user preferences or requirements\n2. Clarify ambiguous instructions\n3. Get decisions on implementation choices as you work\n4. Offer choices to the user about what direction to take\n\nUsage notes:\n- Users can type a custom answer via the automatically appended \"Type something.\" row on every question or press Esc to abandon the questionnaire. Do NOT author \"Other\" or \"Type something.\" labels yourself — reserved labels are rejected at runtime.\n- Use multiSelect: true when multiple answers are valid. The \"Type something.\" row is available on every question, including when options carry a `preview`; in preview mode it expands to the full pane width while typing so the custom answer is not cramped into the narrow options column.\n- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label.\n\nPreview feature:\nUse the optional `preview` field on options when presenting concrete artifacts that users need to visually compare:\n- ASCII mockups of UI layouts or components\n- Code snippets showing different implementations\n- Diagram variations\n- Configuration examples\n\nPreview content is rendered as markdown in a monospace box. Multi-line text with newlines is supported. When any option has a preview, the UI switches to a side-by-side layout with a vertical option list on the left and preview on the right. Do not use previews for simple preference questions where labels and descriptions suffice. Note: previews are only supported for single-select questions (not multiSelect).";

/// Default prompt snippet (`DEFAULT_PROMPT_SNIPPET`).
pub fn default_prompt_snippet() -> String {
    format!(
        "Ask the user up to {MAX_QUESTIONS} structured questions ({MIN_OPTIONS}-{max} options each) when requirements are ambiguous",
        max = crate::tool::types::MAX_OPTIONS
    )
}

/// Default prompt guidelines (`DEFAULT_PROMPT_GUIDELINES`).
pub fn default_prompt_guidelines() -> Vec<String> {
    vec![
        format!(
            "Use ask_user_question whenever the user's request is underspecified and you cannot proceed without concrete decisions — you can ask up to {MAX_QUESTIONS} questions per invocation."
        ),
        format!(
            "Each question MUST have {MIN_OPTIONS}-{max} options. Every option requires a concise label (1-5 words) and a description explaining what the choice means or its trade-offs. The user can additionally type a custom answer via the automatically appended \"Type something.\" row on every question, or press Esc to abandon the questionnaire. Do NOT author \"Other\" or \"Type something.\" labels yourself — reserved labels are rejected at runtime.",
            max = crate::tool::types::MAX_OPTIONS
        ),
        "Set multiSelect: true when multiple answers are valid. Provide an options[].preview markdown string when an option benefits from richer side-by-side context (mockups, code snippets, diagrams, configs) — single-select only. The \"Type something.\" row is appended to every question; in preview mode it expands to the full pane width while typing so the custom answer is not cramped into the narrow options column. If you recommend a specific option, make that the first option and append \"(Recommended)\" to its label.".to_owned(),
        "Do not stack multiple ask_user_question calls back-to-back — group all clarifying questions into one invocation.".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rpi-askq-config-{tag}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_config(&self, dir: &Path, contents: &str) {
            let path = dir.join(CONFIG_DIR_NAME);
            std::fs::create_dir_all(&path).expect("mkdir config dir");
            std::fs::write(path.join(CONFIG_FILE_NAME), contents).expect("write config");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_dir_resolution_matrix() {
        let home = Path::new("/home/user");
        assert_eq!(resolve_config_dir(None, home), home.join(".config"));
        assert_eq!(resolve_config_dir(Some(""), home), home.join(".config"));
        assert_eq!(resolve_config_dir(Some("   "), home), home.join(".config"));
        assert_eq!(
            resolve_config_dir(Some("relative/path"), home),
            home.join(".config")
        );
        assert_eq!(resolve_config_dir(Some("~"), home), home.to_path_buf());
        assert_eq!(resolve_config_dir(Some("~/cfg"), home), home.join("cfg"));
        assert_eq!(
            resolve_config_dir(Some("/abs/cfg"), home),
            PathBuf::from("/abs/cfg")
        );
        // `~user` is not expanded and is not absolute -> default.
        assert_eq!(
            resolve_config_dir(Some("~user/cfg"), home),
            home.join(".config")
        );
    }

    #[test]
    fn config_lookup_order_and_fallbacks() {
        let temp = TempDir::new("lookup");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("home");

        // 3. neither path -> defaults.
        let config = load_config_from(None, &home);
        assert_eq!(config, AskUserQuestionConfig::default());

        // 2. legacy only -> legacy read.
        temp.write_config(
            &home.join(".config"),
            r#"{"collapseKey":"alt+o","guidance":{"description":"legacy"}}"#,
        );
        let config = load_config_from(None, &home);
        assert_eq!(config.collapse_key.as_deref(), Some("alt+o"));
        assert_eq!(config.guidance.description.as_deref(), Some("legacy"));

        // 1. XDG present wins over legacy (and a relative XDG is ignored).
        let xdg = temp.path().join("xdg");
        temp.write_config(&xdg, r#"{"collapseKey":"ctrl+k"}"#);
        let config = load_config_from(Some(xdg.to_str().unwrap()), &home);
        assert_eq!(config.collapse_key.as_deref(), Some("ctrl+k"));
        let config = load_config_from(Some("relative/xdg"), &home);
        assert_eq!(
            config.collapse_key.as_deref(),
            Some("alt+o"),
            "relative XDG falls back to legacy"
        );

        // 1 exists but malformed -> defaults, legacy NOT consulted.
        temp.write_config(&xdg, "{not json");
        let config = load_config_from(Some(xdg.to_str().unwrap()), &home);
        assert_eq!(config, AskUserQuestionConfig::default());

        // 1 exists but valid non-object -> silent defaults.
        temp.write_config(&xdg, "[1,2,3]");
        let config = load_config_from(Some(xdg.to_str().unwrap()), &home);
        assert_eq!(config, AskUserQuestionConfig::default());

        // Single-key type error -> silent fallback for that key only.
        temp.write_config(
            &xdg,
            r#"{"collapseKey":42,"guidance":{"description":"kept"}}"#,
        );
        let config = load_config_from(Some(xdg.to_str().unwrap()), &home);
        assert_eq!(config.collapse_key, None);
        assert_eq!(config.guidance.description.as_deref(), Some("kept"));
    }

    #[test]
    fn config_validate_guidance_fields_matches_upstream() {
        assert_eq!(validate_guidance_fields(None), GuidanceFields::default());
        assert_eq!(
            validate_guidance_fields(Some(&json!("nope"))),
            GuidanceFields::default()
        );
        assert_eq!(
            validate_guidance_fields(Some(&json!({
                "description": "",
                "promptSnippet": 5,
                "promptGuidelines": ["", "ok"]
            }))),
            GuidanceFields::default(),
            "empty/invalid entries are dropped, whole array rejected on a bad element"
        );
        assert_eq!(
            validate_guidance_fields(Some(&json!({
                "description": "d",
                "promptSnippet": "s",
                "promptGuidelines": ["g1", "g2"]
            }))),
            GuidanceFields {
                description: Some("d".to_owned()),
                prompt_snippet: Some("s".to_owned()),
                prompt_guidelines: Some(vec!["g1".to_owned(), "g2".to_owned()]),
            }
        );
        assert_eq!(
            validate_guidance_fields(Some(&json!({"promptGuidelines": []}))),
            GuidanceFields::default(),
            "empty array is rejected"
        );
    }

    #[test]
    fn config_collapse_key_validation_matrix() {
        let config = |key: &str| AskUserQuestionConfig {
            collapse_key: Some(key.to_owned()),
            ..AskUserQuestionConfig::default()
        };
        assert_eq!(
            resolve_collapse_key(&AskUserQuestionConfig::default()),
            "ctrl+]"
        );
        assert_eq!(resolve_collapse_key(&config("")), "ctrl+]");
        assert_eq!(resolve_collapse_key(&config("   ")), "ctrl+]");
        assert_eq!(resolve_collapse_key(&config("OFF")), "off");
        assert_eq!(resolve_collapse_key(&config("Off")), "off");
        assert_eq!(resolve_collapse_key(&config("alt+o")), "alt+o");
        assert_eq!(resolve_collapse_key(&config("  ALT+O  ")), "alt+o");
        assert_eq!(
            resolve_collapse_key(&config("ctrl+shift+h")),
            "ctrl+shift+h"
        );
        assert_eq!(resolve_collapse_key(&config("f9")), "f9");
        assert_eq!(resolve_collapse_key(&config("escape")), "escape");
        assert_eq!(
            resolve_collapse_key(&config("ctrl+pagedown")),
            "ctrl+pagedown"
        );
        // Invalid specs fall back to the default.
        for invalid in [
            "ctr+]",
            "ctrl+ctrl+a",
            "ctrl++a",
            "ctrl+",
            "+a",
            "f13",
            "meta+a",
            "ctrl+ab",
            "CTRL+A+B",
            "ctrl+]x",
        ] {
            assert_eq!(
                resolve_collapse_key(&config(invalid)),
                DEFAULT_COLLAPSE_KEY,
                "{invalid}"
            );
        }
    }

    #[test]
    fn config_format_key_spec_for_display_matrix() {
        assert_eq!(format_key_spec_for_display("ctrl+]"), "Ctrl+]");
        assert_eq!(format_key_spec_for_display("alt+o"), "Alt+O");
        assert_eq!(format_key_spec_for_display("f9"), "F9");
        assert_eq!(
            format_key_spec_for_display("ctrl+pagedown"),
            "Ctrl+PageDown"
        );
        assert_eq!(format_key_spec_for_display("pageup"), "PageUp");
    }

    #[test]
    fn config_default_guidance_text_matches_upstream_shape() {
        let snippet = default_prompt_snippet();
        assert!(snippet.contains("up to 4 structured questions (2-4 options each)"));
        let guidelines = default_prompt_guidelines();
        assert_eq!(guidelines.len(), 4);
        assert!(guidelines[0].contains("up to 4 questions per invocation"));
        assert!(guidelines[1].contains("2-4 options"));
        assert!(guidelines[1].contains("reserved labels are rejected"));
        assert!(guidelines[2].contains("multiSelect: true"));
        assert!(guidelines[3].contains("Do not stack multiple"));
        assert!(DEFAULT_TOOL_DESCRIPTION.contains("Preview feature:"));
        assert!(DEFAULT_TOOL_DESCRIPTION.contains("(Recommended)"));
    }
}
