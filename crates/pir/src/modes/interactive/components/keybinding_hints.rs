//! Keybinding-hint formatting — port of
//! `packages/coding-agent/src/modes/interactive/components/keybinding-hints.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Upstream reads the keybindings global (`getKeybindings()` from
//!   `@earendil-works/pi-tui`); the port reads the pir-side global
//!   [`crate::core::keybindings::get_keybindings`] (the 73-entry table with
//!   `app.*` ids, keybindings.ts:233-244).
//! - [`key_hint`] and [`raw_key_hint`] need the theme for styling; the theme
//!   is passed explicitly instead of read from the global `theme` getter
//!   (theme.ts:799-816).
//! - `formatKeyText` lowercases `ALT` only for the darwin rename; the port
//!   uses `cfg!(target_os = "macos")` (compile-time) exactly like the
//!   keybinding defaults module.

use crate::core::keybindings::get_keybindings;
use crate::core::themes::Theme;

/// `KeyTextFormatOptions` (keybinding-hints.ts:8-11).
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyTextFormatOptions {
    pub capitalize: bool,
}

/// `formatKeyPart` (keybinding-hints.ts:12-15): rename `alt` to `option` on
/// macOS; optionally capitalize the first character.
fn format_key_part(part: &str, options: KeyTextFormatOptions) -> String {
    let display_part = if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
        "option"
    } else {
        part
    };
    if options.capitalize {
        let mut chars = display_part.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    } else {
        display_part.to_string()
    }
}

/// `formatKeyText` (keybinding-hints.ts:17-23): split on `/` (alternative
/// keys) and `+` (chord parts), reformat each part.
pub fn format_key_text(key: &str, options: KeyTextFormatOptions) -> String {
    key.split('/')
        .map(|k| {
            k.split('+')
                .map(|part| format_key_part(part, options))
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `formatKeys` (keybinding-hints.ts:29-32).
fn format_keys(keys: &[String], options: KeyTextFormatOptions) -> String {
    if keys.is_empty() {
        return String::new();
    }
    format_key_text(&keys.join("/"), options)
}

/// `keyText` (keybinding-hints.ts:34-36): the resolved keys for a keybinding
/// id, joined and reformatted (`getKeys().join("/")` upstream).
pub fn key_text(keybinding: &str) -> String {
    let Ok(manager) = get_keybindings().read() else {
        return String::new();
    };
    format_keys(
        &manager.get_keys(keybinding),
        KeyTextFormatOptions::default(),
    )
}

/// `keyDisplayText` (keybinding-hints.ts:38-40): like [`key_text`] with
/// capitalized parts.
pub fn key_display_text(keybinding: &str) -> String {
    let Ok(manager) = get_keybindings().read() else {
        return String::new();
    };
    format_keys(
        &manager.get_keys(keybinding),
        KeyTextFormatOptions { capitalize: true },
    )
}

/// `keyHint` (keybinding-hints.ts:42-44): dim key text + muted description.
pub fn key_hint(theme: &Theme, keybinding: &str, description: &str) -> String {
    format!(
        "{}{}",
        theme.fg("dim", &key_text(keybinding)),
        theme.fg("muted", &format!(" {description}"))
    )
}

/// `rawKeyHint` (keybinding-hints.ts:46-48): like [`key_hint`] for a raw
/// key string instead of a keybinding id.
pub fn raw_key_hint(theme: &Theme, key: &str, description: &str) -> String {
    format!(
        "{}{}",
        theme.fg(
            "dim",
            &format_key_text(key, KeyTextFormatOptions::default())
        ),
        theme.fg("muted", &format!(" {description}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_chords_and_alternatives() {
        assert_eq!(
            format_key_text("ctrl+o", KeyTextFormatOptions::default()),
            "ctrl+o"
        );
        assert_eq!(
            format_key_text("shift+ctrl+p", KeyTextFormatOptions::default()),
            "shift+ctrl+p"
        );
        assert_eq!(
            format_key_text("a/b", KeyTextFormatOptions::default()),
            "a/b"
        );
        assert_eq!(
            format_key_text("ctrl+o", KeyTextFormatOptions { capitalize: true }),
            "Ctrl+O"
        );
    }

    #[test]
    fn alt_is_renamed_to_option_only_on_macos() {
        let expected = if cfg!(target_os = "macos") {
            "option+enter"
        } else {
            "alt+enter"
        };
        assert_eq!(
            format_key_text("alt+enter", KeyTextFormatOptions::default()),
            expected
        );
    }

    #[test]
    fn key_text_resolves_global_defaults() {
        // Defaults only (no user config): app.tools.expand is ctrl+o
        // (keybindings.ts:314).
        assert_eq!(key_text("app.tools.expand"), "ctrl+o");
        assert_eq!(key_text("app.interrupt"), "escape");
        // tui.select.cancel has two defaults (escape, ctrl+c).
        assert_eq!(key_text("tui.select.cancel"), "escape/ctrl+c");
        assert_eq!(key_display_text("app.tools.expand"), "Ctrl+O");
        // Unknown ids resolve to empty (formatKeys returns "").
        assert_eq!(key_text("app.nonexistent"), "");
    }

    #[test]
    fn hints_style_key_and_description() {
        let theme = crate::core::themes::load_theme("dark", None).expect("builtin theme");
        let hint = key_hint(&theme, "app.tools.expand", "to collapse");
        assert!(hint.starts_with('\u{1b}'));
        assert!(hint.contains("ctrl+o"));
        assert!(hint.contains("to collapse"));
        let raw = raw_key_hint(&theme, "ctrl+x", "copy");
        assert!(raw.contains("ctrl+x"));
        assert!(raw.contains(" copy"));
    }
}
