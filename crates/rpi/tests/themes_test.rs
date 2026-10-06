//! Integration tests for `rpi::core::themes`.
//!
//! Port of theme loading/discovery integration scenarios from
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts`.

use std::fs;
use std::path::PathBuf;

use rpi::core::themes::*;

/// Self-managing temp directory (same pattern as other rpi integration tests).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-themes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// --- Built-in theme parsing & construction -------------------------------

#[test]
fn test_builtin_dark_theme_constructs() {
    let theme = load_theme("dark", Some(ColorMode::TrueColor)).unwrap();
    assert_eq!(theme.name.as_deref(), Some("dark"));

    // Foreground colours produce truecolor escapes
    let accent = theme.get_fg_ansi("accent");
    assert!(accent.starts_with("\x1b[38;2;"));

    // Background colours produce truecolor escapes
    let selected_bg = theme.get_bg_ansi("selectedBg");
    assert!(selected_bg.starts_with("\x1b[48;2;"));
}

#[test]
fn test_builtin_light_theme_constructs() {
    let theme = load_theme("light", Some(ColorMode::TrueColor)).unwrap();
    assert_eq!(theme.name.as_deref(), Some("light"));
}

#[test]
fn test_builtin_dark_256_mode() {
    let theme = load_theme("dark", Some(ColorMode::Color256)).unwrap();
    let accent = theme.get_fg_ansi("accent");
    assert!(accent.starts_with("\x1b[38;5;"));
}

#[test]
fn test_builtin_theme_var_resolution() {
    // Dark theme: accent color = "violet" var ref → okhsl(295 50% 67%) →
    // #a798d7 (167,152,215) in the v1.0.0 rewrite (bf8e4b953).
    let theme = load_theme("dark", Some(ColorMode::TrueColor)).unwrap();
    let accent = theme.get_fg_ansi("accent");
    assert_eq!(accent, "\x1b[38;2;167;152;215m");
}

#[test]
fn test_builtin_theme_fg_wraps_text() {
    let theme = load_theme("dark", Some(ColorMode::TrueColor)).unwrap();
    let wrapped = theme.fg("accent", "hello");
    assert!(wrapped.starts_with("\x1b[38;2;"));
    assert!(wrapped.ends_with("\x1b[39m"));
    assert!(wrapped.contains("hello"));
}

#[test]
fn test_builtin_theme_bg_wraps_text() {
    let theme = load_theme("dark", Some(ColorMode::TrueColor)).unwrap();
    let wrapped = theme.bg("selectedBg", "hello");
    assert!(wrapped.starts_with("\x1b[48;2;"));
    assert!(wrapped.ends_with("\x1b[49m"));
    assert!(wrapped.contains("hello"));
}

#[test]
fn test_get_theme_by_name() {
    assert!(get_theme_by_name("dark").is_some());
    assert!(get_theme_by_name("light").is_some());
    assert!(get_theme_by_name("nonexistent").is_none());
}

// --- 51 required token validation ----------------------------------------

#[test]
fn test_all_51_required_tokens_parse_in_builtin_themes() {
    for name in &["dark", "light"] {
        let theme_json = load_theme_json(name).unwrap();
        for key in REQUIRED_COLOR_KEYS {
            assert!(
                theme_json.colors.contains_key(*key),
                "{} theme missing required token: {}",
                name,
                key
            );
        }
    }
}

#[test]
fn test_thinking_max_optional_fallback() {
    // A theme without thinkingMax should fall back to thinkingXhigh
    let mut colors_json = serde_json::Map::new();
    for key in REQUIRED_COLOR_KEYS {
        colors_json.insert((*key).to_string(), serde_json::json!("#000000"));
    }
    // Set thinkingXhigh to a specific value
    colors_json.insert("thinkingXhigh".to_string(), serde_json::json!("#aabbcc"));
    // Don't include thinkingMax

    let theme_json: serde_json::Value = serde_json::json!({
        "name": "fallback-test",
        "colors": serde_json::Value::Object(colors_json),
    });
    let parsed = parse_theme_json("test", &theme_json).unwrap();
    let theme = create_theme(&parsed, Some(ColorMode::TrueColor), None).unwrap();

    // thinkingMax should have thinkingXhigh's value
    let max_ansi = theme.get_fg_ansi("thinkingMax");
    let xhigh_ansi = theme.get_fg_ansi("thinkingXhigh");
    assert_eq!(max_ansi, xhigh_ansi);
}

// --- Custom theme loading -------------------------------------------------

#[test]
fn test_custom_theme_load_from_file() {
    let tmp = TempDir::new();
    let theme_path = tmp.path().join("mytheme.json");

    let mut colors = serde_json::Map::new();
    for key in REQUIRED_COLOR_KEYS {
        colors.insert((*key).to_string(), serde_json::json!("#123456"));
    }
    let theme_json = serde_json::json!({
        "name": "mytheme",
        "colors": serde_json::Value::Object(colors),
    });
    fs::write(&theme_path, serde_json::to_string(&theme_json).unwrap()).unwrap();

    let theme = load_theme_from_path(&theme_path, Some(ColorMode::TrueColor)).unwrap();
    assert_eq!(theme.name.as_deref(), Some("mytheme"));
}

#[test]
fn test_custom_theme_with_vars() {
    let tmp = TempDir::new();
    let theme_path = tmp.path().join("vars-test.json");

    let mut colors = serde_json::Map::new();
    for key in REQUIRED_COLOR_KEYS {
        colors.insert((*key).to_string(), serde_json::json!("myvar"));
    }
    let theme_json = serde_json::json!({
        "name": "vars-test",
        "vars": { "myvar": "#abcdef" },
        "colors": serde_json::Value::Object(colors),
    });
    fs::write(&theme_path, serde_json::to_string(&theme_json).unwrap()).unwrap();

    let theme = load_theme_from_path(&theme_path, Some(ColorMode::TrueColor)).unwrap();
    let accent = theme.get_fg_ansi("accent");
    assert!(accent.contains("171")); // 0xab = 171
}

#[test]
fn test_custom_theme_with_256_color_int() {
    let tmp = TempDir::new();
    let theme_path = tmp.path().join("int-test.json");

    let mut colors = serde_json::Map::new();
    for key in REQUIRED_COLOR_KEYS {
        colors.insert((*key).to_string(), serde_json::json!(39));
    }
    let theme_json = serde_json::json!({
        "name": "int-test",
        "colors": serde_json::Value::Object(colors),
    });
    fs::write(&theme_path, serde_json::to_string(&theme_json).unwrap()).unwrap();

    let theme = load_theme_from_path(&theme_path, Some(ColorMode::TrueColor)).unwrap();
    let accent = theme.get_fg_ansi("accent");
    assert_eq!(accent, "\x1b[38;5;39m");
}

#[test]
fn test_custom_theme_with_empty_string_default() {
    let tmp = TempDir::new();
    let theme_path = tmp.path().join("empty-test.json");

    let mut colors = serde_json::Map::new();
    for key in REQUIRED_COLOR_KEYS {
        colors.insert((*key).to_string(), serde_json::json!(""));
    }
    let theme_json = serde_json::json!({
        "name": "empty-test",
        "colors": serde_json::Value::Object(colors),
    });
    fs::write(&theme_path, serde_json::to_string(&theme_json).unwrap()).unwrap();

    let theme = load_theme_from_path(&theme_path, Some(ColorMode::TrueColor)).unwrap();
    // Empty string = terminal default → reset code
    assert_eq!(theme.get_fg_ansi("accent"), "\x1b[39m");
}

// --- Invalid theme diagnostics (eb3e9feed split) -------------------------
/// The structured "Invalid theme" diagnostics live in the installed
/// validator (`validate_theme_json`, theme-json.ts); the bare library path
/// is lenient (see themes.rs tests). The binary installs the validator at
/// startup (app.rs, main.ts:1004).
#[test]
fn test_missing_colors_diagnostics() {
    let theme_json = serde_json::json!({
        "name": "missing",
        "colors": { "accent": "#ff0000" }
    });
    let result = validate_theme_json("missing", &theme_json);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Missing required color tokens"));
    assert!(err.contains("border"));
    assert!(!err.contains("  - accent")); // accent is present, shouldn't be listed
}

#[test]
fn test_invalid_color_value_diagnostics() {
    let mut colors = serde_json::Map::new();
    for key in REQUIRED_COLOR_KEYS {
        colors.insert((*key).to_string(), serde_json::json!(null));
    }
    let theme_json = serde_json::json!({
        "name": "bad-types",
        "colors": serde_json::Value::Object(colors),
    });
    let result = validate_theme_json("test", &theme_json);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Other errors"));
}

// --- Auto theme -----------------------------------------------------------

#[test]
fn test_auto_theme_full_cycle() {
    // Setting "mylight/mydark"
    let auto = parse_auto_theme_setting(Some("mylight/mydark")).unwrap();
    assert_eq!(auto.light_theme, "mylight");
    assert_eq!(auto.dark_theme, "mydark");

    // Resolving for light terminal → light theme
    let resolved = resolve_theme_setting(Some("mylight/mydark"), TerminalTheme::Light);
    assert_eq!(resolved.as_deref(), Some("mylight"));

    // Resolving for dark terminal → dark theme
    let resolved = resolve_theme_setting(Some("mylight/mydark"), TerminalTheme::Dark);
    assert_eq!(resolved.as_deref(), Some("mydark"));
}

// --- Export colors --------------------------------------------------------

#[test]
fn test_builtin_dark_export_colors() {
    let colors = get_theme_export_colors("dark");
    assert_eq!(colors.page_bg.as_deref(), Some("#21252c"));
    assert_eq!(colors.card_bg.as_deref(), Some("#282c34"));
    assert_eq!(colors.info_bg.as_deref(), Some("#4e2f1b"));
}

#[test]
fn test_builtin_light_export_colors() {
    let colors = get_theme_export_colors("light");
    assert_eq!(colors.page_bg.as_deref(), Some("#efeeee"));
    assert_eq!(colors.card_bg.as_deref(), Some("#f7f6f6"));
    assert_eq!(colors.info_bg.as_deref(), Some("#ede3dd"));
}

// --- Resolved theme colors for HTML export --------------------------------

#[test]
fn test_resolved_theme_colors_dark() {
    let colors = get_resolved_theme_colors("dark").unwrap();
    // All keys should be hex strings (no empty or index values)
    for hex in colors.values() {
        assert!(hex.starts_with('#'), "expected hex, got: {}", hex);
    }
    // thinkingMax should be resolved
    assert!(colors.contains_key("thinkingMax"));
}

/// v0.1.6 review P2-5: `system` is generated (no theme file), so resolving
/// it through the file loader returned an empty map and `/export` produced
/// unstyled HTML. It must resolve like any other theme.
#[test]
fn test_resolved_theme_colors_system_is_not_empty() {
    let colors = get_resolved_theme_colors("system").expect("system theme resolves");
    assert!(!colors.is_empty(), "system theme must resolve colors");
    assert!(colors.contains_key("accent"));
    for hex in colors.values() {
        assert!(hex.starts_with('#'), "expected hex, got: {hex}");
    }
}

// --- COLORFGBG index classification ----------------------------------------

#[test]
fn test_colorfgbg_dark_background() {
    // COLORFGBG "0;0" = black background = dark
    let det = detect_terminal_background_from_env_str("0;0");
    assert_eq!(det.theme, TerminalTheme::Dark);
    assert_eq!(det.confidence, TerminalThemeConfidence::High);
}

#[test]
fn test_colorfggb_light_background() {
    // COLORFGBG "0;15" = white background (index 15) = light
    let det = detect_terminal_background_from_env_str("0;15");
    assert_eq!(det.theme, TerminalTheme::Light);
}

#[test]
fn test_colorfgbg_index_eight_is_dark_and_seven_is_light() {
    // Vim-style classification (theme.ts:697-705 @ a13d35a74).
    let det = detect_terminal_background_from_env_str("15;8");
    assert_eq!(det.theme, TerminalTheme::Dark);
    let det = detect_terminal_background_from_env_str("0;7");
    assert_eq!(det.theme, TerminalTheme::Light);
    // An out-of-palette index falls back to dark.
    let det = detect_terminal_background_from_env_str("0;235");
    assert_eq!(det.source, TerminalThemeSource::Fallback);
}

#[test]
fn test_colorfgbg_no_env_fallback() {
    let det = detect_terminal_background_from_env_str("");
    assert_eq!(det.theme, TerminalTheme::Dark);
    assert_eq!(det.source, TerminalThemeSource::Fallback);
    assert_eq!(det.confidence, TerminalThemeConfidence::Low);
}

// --- Name validation ------------------------------------------------------

#[test]
fn test_theme_name_slash_rejected() {
    assert!(assert_theme_name_is_valid("ok").is_ok());
    assert!(assert_theme_name_is_valid("foo/bar").is_err());
}

// --- Color mode property --------------------------------------------------

#[test]
fn test_theme_color_mode() {
    let theme_truecolor = load_theme("dark", Some(ColorMode::TrueColor)).unwrap();
    assert_eq!(theme_truecolor.get_color_mode(), ColorMode::TrueColor);

    let theme_256 = load_theme("dark", Some(ColorMode::Color256)).unwrap();
    assert_eq!(theme_256.get_color_mode(), ColorMode::Color256);
}
