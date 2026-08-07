//! Port of `packages/coding-agent/src/core/export-html/index.ts` @ pi 0.82.1
//! (2efa728): HTML session export.
//!
//! The exported HTML embeds the upstream viewer assets byte-for-byte
//! (`template.html` / `template.css` / `template.js` / `vendor/*.min.js`,
//! copied from `external/pi/packages/coding-agent/src/core/export-html/`) via
//! `include_str!`; a test pins that byte-parity. The viewer renders the
//! session client-side from a base64-encoded JSON `SessionData` blob, so the
//! Rust port's contract is the substitution order, the `SessionData` shape,
//! the theme-var generation and the file naming / error messages.
//!
//! Intentional differences (vs upstream):
//! - Upstream reads the template files from the package directory at runtime
//!   (`getExportTemplateDir`, config.ts:412-419); pir embeds them at compile
//!   time (single-file binary, ADR-0002 §8). There is no
//!   `PIR_PACKAGE_DIR`-style override for the template.
//! - `ToolHtmlRenderer` / `renderedTools` pre-rendering (index.ts:15-33,
//!   183-230) is not ported: it runs extension TUI renderers through an
//!   ANSI→HTML pipeline (`tool-renderer.ts` / `ansi-to-html.ts`), and pir has
//!   no JS extension renderers. `renderedTools` is always omitted; the
//!   viewer's generic tool-call fallback renders those calls instead
//!   (template.js `renderedTools?.[call.id]`).
//! - Theme colours iterate a `HashMap` upstream-port (`get_resolved_theme_colors`);
//!   the `--key: value;` lines are emitted in **sorted key order** for
//!   determinism, while upstream emits the theme JSON's insertion order.
//! - The `themeName ?? currentThemeName ?? getDefaultTheme()` fallback
//!   (theme.ts:1023) drops the `currentThemeName` global (no theme globals
//!   in pir): `None` resolves straight to the terminal-detected default.
//! - Async upstream signatures are synchronous here (pure CPU + file IO).

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use crate::config::APP_NAME;
use crate::core::session_manager::SessionManager;
use crate::core::themes::{get_default_theme, get_resolved_theme_colors, get_theme_export_colors};
use crate::error::PirError;
use crate::tools::path_utils::{normalize_path, resolve_path};

/// `template.html` (export-html/template.html) — byte-identical copy.
const TEMPLATE_HTML: &str = include_str!("export_html/template.html");
/// `template.css` (export-html/template.css) — byte-identical copy.
const TEMPLATE_CSS: &str = include_str!("export_html/template.css");
/// `template.js` (export-html/template.js) — byte-identical copy.
const TEMPLATE_JS: &str = include_str!("export_html/template.js");
/// `vendor/marked.min.js` — byte-identical copy.
const MARKED_JS: &str = include_str!("export_html/vendor/marked.min.js");
/// `vendor/highlight.min.js` — byte-identical copy.
const HIGHLIGHT_JS: &str = include_str!("export_html/vendor/highlight.min.js");

/// `ExportOptions` (index.ts:35-40) minus the `toolRenderer` hook (see the
/// module header).
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub output_path: Option<String>,
    pub theme_name: Option<String>,
}

impl ExportOptions {
    /// Upstream accepts `ExportOptions | string` (index.ts:241): a bare
    /// string is the output path.
    pub fn from_output_path(output_path: Option<&str>) -> Self {
        ExportOptions {
            output_path: output_path.map(str::to_owned),
            theme_name: None,
        }
    }
}

/// A tool's export view — `Pick<ToolDefinition, "name" | "description" |
/// "parameters">` (index.ts:135).
#[derive(Debug, Clone, Serialize)]
pub struct ExportToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// `SessionData` (index.ts:130-138). `undefined` fields are dropped by
/// `JSON.stringify`; the serde `skip_serializing_if` mirrors that. Key order
/// follows the upstream object literal (`preserve_order`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionData {
    header: Value,
    entries: Vec<Value>,
    leaf_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ExportToolInfo>>,
    // `renderedTools` is never emitted (see the module header).
}

/// `{ r, g, b }` (index.ts:43).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RgbColor {
    r: u32,
    g: u32,
    b: u32,
}

/// `parseColor` (index.ts:43-61): `#RRGGBB` or `rgb(r,g,b)`.
fn parse_color(color: &str) -> Option<RgbColor> {
    let hex = color.strip_prefix('#');
    if let Some(hex) = hex {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(RgbColor {
                r: u32::from_str_radix(&hex[0..2], 16).ok()?,
                g: u32::from_str_radix(&hex[2..4], 16).ok()?,
                b: u32::from_str_radix(&hex[4..6], 16).ok()?,
            });
        }
        return None;
    }
    // `^rgb\s*\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$`
    let inner = color.strip_prefix("rgb")?.trim_start();
    let inner = inner.strip_prefix('(')?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut rgb = [0u32; 3];
    for (i, part) in parts.iter().enumerate() {
        let part = part.trim();
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        rgb[i] = part.parse().ok()?;
    }
    Some(RgbColor {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
    })
}

/// `getLuminance` (index.ts:64-70): relative luminance, 0–1.
fn get_luminance(rgb: RgbColor) -> f64 {
    let to_linear = |c: u32| -> f64 {
        let s = c as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055_f64).powf(2.4)
        }
    };
    0.2126 * to_linear(rgb.r) + 0.7152 * to_linear(rgb.g) + 0.0722 * to_linear(rgb.b)
}

/// `adjustBrightness` (index.ts:73-78). Factor > 1 lightens, < 1 darkens.
/// Unparseable colours pass through unchanged.
fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some(parsed) = parse_color(color) else {
        return color.to_string();
    };
    let adjust = |c: u32| -> u32 { ((c as f64 * factor).round()).clamp(0.0, 255.0) as u32 };
    format!(
        "rgb({}, {}, {})",
        adjust(parsed.r),
        adjust(parsed.g),
        adjust(parsed.b)
    )
}

/// `{ pageBg, cardBg, infoBg }` (index.ts:80).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportBgColors {
    page_bg: String,
    card_bg: String,
    info_bg: String,
}

/// `deriveExportColors` (index.ts:81-106): derive export background colours
/// from a base colour (e.g. `userMessageBg`).
fn derive_export_colors(base_color: &str) -> ExportBgColors {
    let Some(parsed) = parse_color(base_color) else {
        return ExportBgColors {
            page_bg: "rgb(24, 24, 30)".to_string(),
            card_bg: "rgb(30, 30, 36)".to_string(),
            info_bg: "rgb(60, 55, 40)".to_string(),
        };
    };

    let luminance = get_luminance(parsed);
    let is_light = luminance > 0.5;

    if is_light {
        return ExportBgColors {
            page_bg: adjust_brightness(base_color, 0.96),
            card_bg: base_color.to_string(),
            info_bg: format!(
                "rgb({}, {}, {})",
                (parsed.r + 10).min(255),
                (parsed.g + 5).min(255),
                parsed.b.saturating_sub(20)
            ),
        };
    }
    ExportBgColors {
        page_bg: adjust_brightness(base_color, 0.7),
        card_bg: adjust_brightness(base_color, 0.85),
        info_bg: format!(
            "rgb({}, {}, {})",
            (parsed.r + 20).min(255),
            (parsed.g + 15).min(255),
            parsed.b
        ),
    }
}

/// The upstream `themeName ?? getDefaultTheme()` resolution (theme.ts:1023);
/// the `currentThemeName` global is not ported (module header).
fn resolve_theme_name(theme_name: Option<&str>) -> String {
    match theme_name {
        Some(name) => name.to_string(),
        None => get_default_theme().as_str().to_string(),
    }
}

/// `generateThemeVars` (index.ts:111-128): CSS custom property declarations
/// for the theme. Lines are emitted in sorted key order (module header) and
/// joined with `"\n      "` like upstream.
///
/// Divergence (T14 review): an unknown theme name falls back to an empty
/// color map + derived colors instead of upstream's throw (`loadThemeJson`
/// rejects with "Theme not found"). Practically unreachable — the CLI
/// passes no theme (defaults are always built-in) and the interactive path
/// filters via `get_theme_by_name` — so the fallback is kept for a total
/// function.
fn generate_theme_vars(theme_name: Option<&str>) -> String {
    let name = resolve_theme_name(theme_name);
    let colors = get_resolved_theme_colors(&name).unwrap_or_default();
    let mut keys: Vec<&String> = colors.keys().collect();
    keys.sort();
    let mut lines: Vec<String> = keys
        .into_iter()
        .map(|key| format!("--{}: {};", key, colors[key]))
        .collect();

    // Use explicit theme export colors if available, otherwise derive from
    // userMessageBg (index.ts:118-125).
    let theme_export = get_theme_export_colors(&name);
    let user_message_bg = colors
        .get("userMessageBg")
        .map(String::as_str)
        .unwrap_or("#343541");
    let derived = derive_export_colors(user_message_bg);

    lines.push(format!(
        "--exportPageBg: {};",
        theme_export.page_bg.as_deref().unwrap_or(&derived.page_bg)
    ));
    lines.push(format!(
        "--exportCardBg: {};",
        theme_export.card_bg.as_deref().unwrap_or(&derived.card_bg)
    ));
    lines.push(format!(
        "--exportInfoBg: {};",
        theme_export.info_bg.as_deref().unwrap_or(&derived.info_bg)
    ));

    lines.join("\n      ")
}

/// `generateHtml` (index.ts:143-175): substitute the placeholders in the
/// same order as upstream (JS `String.replace` replaces the first
/// occurrence; `replacen(…, 1)` matches).
fn generate_html(session_data: &SessionData, theme_name: Option<&str>) -> Result<String, PirError> {
    let name = resolve_theme_name(theme_name);
    let theme_vars = generate_theme_vars(Some(&name));
    // Unknown theme → empty map (see `generate_theme_vars` divergence
    // note; both call sites here are the same unreachable-in-practice path).
    let colors = get_resolved_theme_colors(&name).unwrap_or_default();
    let theme_export = get_theme_export_colors(&name);
    let derived = derive_export_colors(
        colors
            .get("userMessageBg")
            .map(String::as_str)
            .unwrap_or("#343541"),
    );
    let body_bg = theme_export.page_bg.as_deref().unwrap_or(&derived.page_bg);
    let container_bg = theme_export.card_bg.as_deref().unwrap_or(&derived.card_bg);
    let info_bg = theme_export.info_bg.as_deref().unwrap_or(&derived.info_bg);

    // Base64 encode session data to avoid escaping issues (index.ts:159-160).
    let session_data_base64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_string(session_data)?);

    let css = TEMPLATE_CSS
        .replacen("{{THEME_VARS}}", &theme_vars, 1)
        .replacen("{{BODY_BG}}", body_bg, 1)
        .replacen("{{CONTAINER_BG}}", container_bg, 1)
        .replacen("{{INFO_BG}}", info_bg, 1);

    Ok(TEMPLATE_HTML
        .replacen("{{CSS}}", &css, 1)
        .replacen("{{JS}}", TEMPLATE_JS, 1)
        .replacen("{{SESSION_DATA}}", &session_data_base64, 1)
        .replacen("{{MARKED_JS}}", MARKED_JS, 1)
        .replacen("{{HIGHLIGHT_JS}}", HIGHLIGHT_JS, 1))
}

/// `basename(path, ".jsonl")`: strip the `.jsonl` suffix only.
fn session_basename(file: &Path) -> String {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.strip_suffix(".jsonl").unwrap_or(&name).to_string()
}

/// The default output name (index.ts:274-278, 308-312):
/// `{APP_NAME}-session-{basename}.html`, relative to the cwd.
fn default_output_path(session_file: &Path) -> String {
    format!("{APP_NAME}-session-{}.html", session_basename(session_file))
}

/// `exportSessionToHtml` (index.ts:236-282): export the session behind a
/// [`SessionManager`] to HTML. `system_prompt` / `tools` are the
/// `state?.systemPrompt` / `state?.tools` slices (absent for
/// [`export_from_file`]).
pub fn export_session_to_html(
    sm: &SessionManager,
    system_prompt: Option<String>,
    tools: Option<Vec<ExportToolInfo>>,
    options: &ExportOptions,
) -> Result<String, PirError> {
    let session_file = sm
        .get_session_file()
        .ok_or_else(|| PirError::Session("Cannot export in-memory session to HTML".to_string()))?;
    if !session_file.exists() {
        return Err(PirError::Session(
            "Nothing to export yet - start a conversation first".to_string(),
        ));
    }

    // Raw header object (upstream `getHeader()` returns the parsed object
    // as-is — unknown extension fields survive; T14 review fix — the
    // previous typed re-serialization dropped them).
    let header = match sm.get_header_raw() {
        Some(header) => header.clone(),
        None => Value::Null,
    };
    let entries: Vec<Value> = sm
        .get_entries()
        .iter()
        .map(|entry| entry.raw_value().clone())
        .collect();
    let session_data = SessionData {
        header,
        entries,
        leaf_id: sm.get_leaf_id().map(str::to_owned),
        system_prompt,
        tools,
    };

    let html = generate_html(&session_data, options.theme_name.as_deref())?;

    let output_path = match &options.output_path {
        Some(path) => normalize_path(path),
        None => default_output_path(session_file),
    };
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

/// `exportFromFile` (index.ts:288-316): export an arbitrary session file
/// (standalone, without agent state). Used by the `--export` CLI path.
pub fn export_from_file(input_path: &str, options: &ExportOptions) -> Result<String, PirError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolved_input = resolve_path(input_path, &cwd);
    if !resolved_input.exists() {
        return Err(PirError::Session(format!(
            "File not found: {}",
            resolved_input.display()
        )));
    }

    let sm = SessionManager::open(&resolved_input, None, None)?;
    export_session_to_html(&sm, None, None, options)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // parseColor / adjustBrightness / deriveExportColors (index.ts:43-106)
    // ---------------------------------------------------------------

    #[test]
    fn parse_color_hex() {
        assert_eq!(
            parse_color("#343541"),
            Some(RgbColor {
                r: 0x34,
                g: 0x35,
                b: 0x41
            })
        );
        assert_eq!(
            parse_color("#ABCabc"),
            Some(RgbColor {
                r: 0xab,
                g: 0xca,
                b: 0xbc
            })
        );
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#1234567"), None);
        assert_eq!(parse_color("#zz0000"), None);
        assert_eq!(parse_color("343541"), None);
    }

    #[test]
    fn parse_color_rgb() {
        assert_eq!(
            parse_color("rgb(1, 2, 3)"),
            Some(RgbColor { r: 1, g: 2, b: 3 })
        );
        assert_eq!(
            parse_color("rgb( 10 ,20,30 )"),
            Some(RgbColor {
                r: 10,
                g: 20,
                b: 30
            })
        );
        assert_eq!(parse_color("rgb(1, 2)"), None);
        assert_eq!(parse_color("rgb(1, 2, 3, 4)"), None);
        assert_eq!(parse_color("rgba(1,2,3)"), None);
        assert_eq!(parse_color("rgb(-1,2,3)"), None);
    }

    #[test]
    fn adjust_brightness_clamps_and_rounds() {
        assert_eq!(adjust_brightness("#343541", 0.7), "rgb(36, 37, 46)");
        assert_eq!(adjust_brightness("#ffffff", 1.5), "rgb(255, 255, 255)");
        assert_eq!(adjust_brightness("#010101", 0.1), "rgb(0, 0, 0)");
        // Unparseable → passthrough (index.ts:75).
        assert_eq!(adjust_brightness("red", 0.7), "red");
    }

    #[test]
    fn derive_export_colors_unparseable_fallback() {
        assert_eq!(
            derive_export_colors("not-a-color"),
            ExportBgColors {
                page_bg: "rgb(24, 24, 30)".to_string(),
                card_bg: "rgb(30, 30, 36)".to_string(),
                info_bg: "rgb(60, 55, 40)".to_string(),
            }
        );
    }

    #[test]
    fn derive_export_colors_dark_base() {
        // #343541 luminance < 0.5 → dark branch (index.ts:101-105).
        let derived = derive_export_colors("#343541");
        assert_eq!(derived.page_bg, adjust_brightness("#343541", 0.7));
        assert_eq!(derived.card_bg, adjust_brightness("#343541", 0.85));
        assert_eq!(derived.info_bg, "rgb(72, 68, 65)");
    }

    #[test]
    fn derive_export_colors_light_base() {
        // #e8e8e8 luminance > 0.5 → light branch (index.ts:94-99).
        let derived = derive_export_colors("#e8e8e8");
        assert_eq!(derived.page_bg, adjust_brightness("#e8e8e8", 0.96));
        assert_eq!(derived.card_bg, "#e8e8e8");
        assert_eq!(derived.info_bg, "rgb(242, 237, 212)");
    }

    // ---------------------------------------------------------------
    // generateThemeVars (index.ts:111-128)
    // ---------------------------------------------------------------

    #[test]
    fn theme_vars_dark_uses_explicit_export_colors() {
        let vars = generate_theme_vars(Some("dark"));
        assert!(vars.contains("--userMessageBg: #343541;"), "vars: {vars}");
        // The dark theme pins explicit export colors (themes.rs dark JSON).
        assert!(vars.contains("--exportPageBg: #18181e;"), "vars: {vars}");
        assert!(vars.contains("--exportCardBg: #1e1e24;"), "vars: {vars}");
        assert!(vars.contains("--exportInfoBg: #3c3728;"), "vars: {vars}");
        // Joined with newline + 6 spaces (index.ts:127).
        assert!(vars.contains("\n      --"), "join separator: {vars}");
    }

    #[test]
    fn theme_vars_unknown_theme_derives_from_fallback_bg() {
        // Unknown theme → no colors → userMessageBg falls back to #343541
        // (index.ts:120) and export colors are derived (index.ts:84-89 is
        // not hit because #343541 parses).
        let vars = generate_theme_vars(Some("no-such-theme"));
        let derived = derive_export_colors("#343541");
        assert!(vars.contains(&format!("--exportPageBg: {};", derived.page_bg)));
        assert!(vars.contains(&format!("--exportCardBg: {};", derived.card_bg)));
        assert!(vars.contains(&format!("--exportInfoBg: {};", derived.info_bg)));
    }

    // ---------------------------------------------------------------
    // generateHtml structure (index.ts:143-175)
    // ---------------------------------------------------------------

    fn sample_session_data() -> SessionData {
        SessionData {
            header: serde_json::json!({"type": "session", "version": 3, "id": "s1",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp"}),
            entries: vec![serde_json::json!({"type": "message", "id": "e1"})],
            leaf_id: Some("e1".to_string()),
            system_prompt: Some("prompt".to_string()),
            tools: Some(vec![ExportToolInfo {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
        }
    }

    /// Extract and decode the embedded `SessionData` blob.
    fn decode_session_data(html: &str) -> Value {
        let marker = r#"<script id="session-data" type="application/json">"#;
        let start = html.find(marker).expect("session-data script") + marker.len();
        let end = html[start..].find("</script>").expect("script close") + start;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&html[start..end])
            .expect("base64");
        serde_json::from_slice(&bytes).expect("session data json")
    }

    #[test]
    fn generate_html_substitutes_all_placeholders() {
        let html = generate_html(&sample_session_data(), Some("dark")).expect("html");
        for placeholder in [
            "{{CSS}}",
            "{{JS}}",
            "{{SESSION_DATA}}",
            "{{MARKED_JS}}",
            "{{HIGHLIGHT_JS}}",
            "{{THEME_VARS}}",
            "{{BODY_BG}}",
            "{{CONTAINER_BG}}",
            "{{INFO_BG}}",
        ] {
            assert!(!html.contains(placeholder), "leftover {placeholder}");
        }
        // Template skeleton survives (template.html structure).
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains(r#"<div id="messages"></div>"#));
        assert!(html.contains(r#"<div class="tree-container" id="tree-container"></div>"#));
        // Vendored libraries inlined.
        assert!(html.contains("marked"), "marked vendored");
        // Theme export colors land in the CSS (dark theme explicit values).
        assert!(html.contains("--exportPageBg: #18181e;"));
        assert!(html.contains("--body-bg: #18181e;"), "body bg substituted");
    }

    #[test]
    fn generate_html_session_data_roundtrip() {
        let html = generate_html(&sample_session_data(), Some("dark")).expect("html");
        let data = decode_session_data(&html);
        assert_eq!(data["header"]["type"], "session");
        assert_eq!(data["header"]["id"], "s1");
        assert_eq!(data["entries"][0]["id"], "e1");
        assert_eq!(data["leafId"], "e1");
        assert_eq!(data["systemPrompt"], "prompt");
        assert_eq!(data["tools"][0]["name"], "read");
        assert!(data.get("renderedTools").is_none(), "never emitted");
    }

    #[test]
    fn generate_html_drops_absent_state_fields() {
        let mut data = sample_session_data();
        data.system_prompt = None;
        data.tools = None;
        data.leaf_id = None;
        let html = generate_html(&data, Some("dark")).expect("html");
        let value = decode_session_data(&html);
        // JSON.stringify keeps explicit nulls but drops undefined
        // (index.ts:263-270 with no state).
        assert!(value.get("systemPrompt").is_none());
        assert!(value.get("tools").is_none());
        assert!(value["leafId"].is_null());
    }

    // ---------------------------------------------------------------
    // Embedded asset parity with the pinned upstream
    // (covers the intent of test/export-html-xss.test.ts etc.: the
    // viewer JS carries its upstream sanitization verbatim).
    // ---------------------------------------------------------------

    fn upstream_asset(rel: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("external/pi/packages/coding-agent/src/core/export-html")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    #[test]
    fn embedded_assets_match_upstream_byte_for_byte() {
        assert_eq!(TEMPLATE_HTML, upstream_asset("template.html"));
        assert_eq!(TEMPLATE_CSS, upstream_asset("template.css"));
        assert_eq!(TEMPLATE_JS, upstream_asset("template.js"));
        assert_eq!(MARKED_JS, upstream_asset("vendor/marked.min.js"));
        assert_eq!(HIGHLIGHT_JS, upstream_asset("vendor/highlight.min.js"));
    }

    // ---------------------------------------------------------------
    // Naming / errors
    // ---------------------------------------------------------------

    #[test]
    fn session_basename_strips_only_jsonl() {
        assert_eq!(session_basename(Path::new("/a/b/foo.jsonl")), "foo");
        assert_eq!(session_basename(Path::new("/a/b/foo.bar")), "foo.bar");
        assert_eq!(session_basename(Path::new("/a/b/foo.bar.jsonl")), "foo.bar");
    }

    #[test]
    fn default_output_name_uses_app_name() {
        assert_eq!(
            default_output_path(Path::new("/a/b/foo.jsonl")),
            "pir-session-foo.html"
        );
    }

    // ---------------------------------------------------------------
    // exportSessionToHtml / exportFromFile (index.ts:236-316)
    // ---------------------------------------------------------------

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pir-export-html-test-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const TS: &str = "2025-01-01T00:00:00.000Z";

    /// A minimal v3 session file: header + one user message.
    fn write_session_file(dir: &Path, name: &str) -> PathBuf {
        let file = dir.join(name);
        let content = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"sess-1\",\"timestamp\":\"{TS}\",\"cwd\":\"/tmp\"}}\n\
             {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"{TS}\",\"message\":{{\"role\":\"user\",\"content\":\"hello export\",\"timestamp\":1}}}}\n"
        );
        std::fs::write(&file, content).expect("write session file");
        file
    }

    #[test]
    fn export_from_file_writes_html_with_session_data() {
        let tmp = TempDir::new();
        let session_file = write_session_file(tmp.path(), "s.jsonl");
        let output = tmp.path().join("out.html");
        let result = export_from_file(
            &session_file.to_string_lossy(),
            &ExportOptions::from_output_path(Some(&output.to_string_lossy())),
        )
        .expect("export");
        assert_eq!(result, output.to_string_lossy());

        let html = std::fs::read_to_string(&output).expect("read html");
        let data = decode_session_data(&html);
        assert_eq!(data["header"]["type"], "session");
        assert_eq!(data["header"]["id"], "sess-1");
        assert_eq!(data["entries"].as_array().expect("entries").len(), 1);
        assert_eq!(data["entries"][0]["message"]["content"], "hello export");
        assert_eq!(data["leafId"], "m1");
        // Standalone export has no agent state (index.ts:298-304).
        assert!(data.get("systemPrompt").is_none());
        assert!(data.get("tools").is_none());
    }

    #[test]
    fn export_from_file_missing_file_errors() {
        let tmp = TempDir::new();
        let missing = tmp.path().join("nope.jsonl");
        let err = export_from_file(&missing.to_string_lossy(), &ExportOptions::default())
            .expect_err("must fail");
        assert_eq!(
            err.raw_message(),
            format!("File not found: {}", missing.display())
        );
    }

    #[test]
    fn export_session_to_html_rejects_in_memory_session() {
        let sm = SessionManager::in_memory(None, Default::default()).expect("in-memory");
        let err = export_session_to_html(&sm, None, None, &ExportOptions::default())
            .expect_err("must fail");
        assert_eq!(err.raw_message(), "Cannot export in-memory session to HTML");
    }

    #[test]
    fn export_session_to_html_rejects_empty_session_file() {
        // File-backed session whose file was never written
        // (index.ts:247-249).
        let tmp = TempDir::new();
        let sm = SessionManager::create(tmp.path(), Some(tmp.path()), Default::default())
            .expect("create");
        let err = export_session_to_html(&sm, None, None, &ExportOptions::default())
            .expect_err("must fail");
        assert_eq!(
            err.raw_message(),
            "Nothing to export yet - start a conversation first"
        );
    }

    #[test]
    fn export_session_to_html_default_name_from_session_file() {
        let tmp = TempDir::new();
        let session_file = write_session_file(tmp.path(), "abc.jsonl");
        let sm = SessionManager::open(&session_file, None, None).expect("open");
        // Default name lands in the process cwd — run the export with a
        // scoped cwd change serialized against other cwd users... instead
        // just assert the naming helper the branch delegates to.
        assert_eq!(
            default_output_path(sm.get_session_file().expect("file")),
            "pir-session-abc.html"
        );
    }
}
