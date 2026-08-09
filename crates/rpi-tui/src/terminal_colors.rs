//! OSC 11 background color and color scheme report parsing (terminal-colors.ts).
//!
//! Port of `packages/tui/src/terminal-colors.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

/// `RgbColor` (terminal-colors.ts:1-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// `TerminalColorScheme` (terminal-colors.ts:7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorScheme {
    Dark,
    Light,
}

/// `hexToRgb` (terminal-colors.ts:9-15); caller guarantees a `#rrggbb` value.
fn hex_to_rgb(hex: &str) -> Option<RgbColor> {
    let hex = hex.strip_prefix('#')?;
    let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
    let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
    let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
    Some(RgbColor { r, g, b })
}

/// `parseOscHexChannel` (terminal-colors.ts:17-26).
fn parse_osc_hex_channel(channel: &str) -> Option<u8> {
    // /^[0-9a-f]+$/i
    if channel.is_empty() || !channel.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16_f64.powi(channel.len() as i32) - 1.0;
    if max <= 0.0 {
        return None;
    }
    let value = u64::from_str_radix(channel, 16).ok()?;
    Some(((value as f64 / max) * 255.0).round() as u8)
}

/// Strips the optional `rgb:`/`rgba:` prefix, case-insensitively
/// (`value.replace(/^rgba?:/i, "")`, terminal-colors.ts:56).
fn strip_rgb_prefix(value: &str) -> &str {
    for prefix in ["rgb:", "rgba:"] {
        if value
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return &value[prefix.len()..];
        }
    }
    value
}

/// Matches `/^\x1b\]11;([^\x07\x1b]*)(?:\x07|\x1b\\)$/i`
/// (terminal-colors.ts:28) and returns the captured value. The capture stops at
/// the first terminator char, which must then be the very end of the response.
fn parse_osc11_background_color_match(data: &str) -> Option<&str> {
    let rest = data.strip_prefix("\x1b]11;")?;
    let terminator_pos = rest.find(['\x07', '\x1b'])?;
    let (value, terminator) = rest.split_at(terminator_pos);
    match terminator {
        "\x07" | "\x1b\\" => Some(value),
        _ => None,
    }
}

/// `isOsc11BackgroundColorResponse` (terminal-colors.ts:31-33).
pub fn is_osc11_background_color_response(data: &str) -> bool {
    parse_osc11_background_color_match(data).is_some()
}

/// `parseOsc11BackgroundColor` (terminal-colors.ts:35-65).
pub fn parse_osc11_background_color(data: &str) -> Option<RgbColor> {
    let value = parse_osc11_background_color_match(data)?.trim();

    if let Some(hex) = value.strip_prefix('#') {
        // /^[0-9a-f]{6}$/i
        if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return hex_to_rgb(value);
        }
        // /^[0-9a-f]{12}$/i — 16-bit-per-channel hex (e.g. `#00008000ffff`)
        if hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let r = parse_osc_hex_channel(hex.get(0..4)?)?;
            let g = parse_osc_hex_channel(hex.get(4..8)?)?;
            let b = parse_osc_hex_channel(hex.get(8..12)?)?;
            return Some(RgbColor { r, g, b });
        }
        return None;
    }

    // `rgb:`/`rgba:` responses (e.g. `rgb:0000/8000/ffff`); at least three
    // slash-separated channels, extra parts ignored (JS array destructuring).
    let rgb_value = strip_rgb_prefix(value);
    let mut parts = rgb_value.split('/');
    let r = parse_osc_hex_channel(parts.next()?)?;
    let g = parse_osc_hex_channel(parts.next()?)?;
    let b = parse_osc_hex_channel(parts.next()?)?;
    Some(RgbColor { r, g, b })
}

/// `parseTerminalColorSchemeReport` (terminal-colors.ts:67-73). The upstream
/// regex has no flags, so the match is case-sensitive: a trailing `N` does not
/// match.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    if data == "\x1b[?997;1n" {
        Some(TerminalColorScheme::Dark)
    } else if data == "\x1b[?997;2n" {
        Some(TerminalColorScheme::Light)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_osc11_background_color_parses_16bit_rgb_responses() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;rgb:0000/8000/ffff\x07"),
            Some(RgbColor {
                r: 0,
                g: 128,
                b: 255
            })
        );
    }

    #[test]
    fn test_parse_osc11_background_color_parses_hex_responses() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#ffffff\x1b\\"),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#000000\x07"),
            Some(RgbColor { r: 0, g: 0, b: 0 })
        );
    }

    #[test]
    fn test_parse_osc11_background_color_rejects_non_strict_responses() {
        assert_eq!(parse_osc11_background_color("x\x1b]11;#ffffff\x07"), None);
        assert_eq!(parse_osc11_background_color("\x1b]10;#ffffff\x07"), None);
        assert_eq!(parse_osc11_background_color("\x1b]11;#ffffff\x07x"), None);
    }

    #[test]
    fn test_parse_terminal_color_scheme_report_parses_reports() {
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?996n"), None);
        assert_eq!(parse_terminal_color_scheme_report("x\x1b[?997;1n"), None);
    }

    // Supplementary coverage (upstream tests these functions only indirectly
    // through the TUI core, which is ported in a later phase).

    #[test]
    fn test_is_osc11_background_color_response() {
        assert!(is_osc11_background_color_response("\x1b]11;#ffffff\x07"));
        assert!(is_osc11_background_color_response(
            "\x1b]11;rgb:0000/8000/ffff\x1b\\"
        ));
        assert!(!is_osc11_background_color_response("\x1b]11;#ffffff\x07x"));
        assert!(!is_osc11_background_color_response("plain text"));
    }

    #[test]
    fn test_parse_osc11_background_color_accepts_case_insensitive_hex_and_rgb_prefix() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#FFAABB\x07"),
            Some(RgbColor {
                r: 255,
                g: 170,
                b: 187
            })
        );
        assert_eq!(
            parse_osc11_background_color("\x1b]11;RGB:ff00/ff00/ff00\x07"),
            // 4-digit channels: 0xff00 / 0xffff * 255 = 254.0088 → round = 254.
            Some(RgbColor {
                r: 254,
                g: 254,
                b: 254
            })
        );
    }

    #[test]
    fn test_parse_osc11_background_color_parses_12_digit_hex_channels() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11;#00008000ffff\x07"),
            Some(RgbColor {
                r: 0,
                g: 128,
                b: 255
            })
        );
    }

    #[test]
    fn test_parse_osc11_background_color_trims_whitespace_around_value() {
        assert_eq!(
            parse_osc11_background_color("\x1b]11; #ffffff \x07"),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
    }

    #[test]
    fn test_parse_terminal_color_scheme_report_rejects_uppercase_n() {
        // Upstream `/^\x1b\[\?997;(1|2)n$/` has no flags — a trailing `N`
        // must not match.
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;1N"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;2N"), None);
    }
}
