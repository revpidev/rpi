//! OSC 10/11/4 color replies and color-scheme report parsing
//! (terminal-colors.ts @ a13d35a74).
//!
//! Port of `packages/tui/src/terminal-colors.ts` @ pi v1.0.0 (a13d35a74),
//! plus the `RgbColor` / `TerminalColors` / `OscColorTarget` types consumed by
//! the single-pass `queryTerminalColors()` query (tui.ts:1470). The two
//! legacy query faces (`parseOsc11BackgroundColor` and the per-face query
//! methods) were removed upstream in 0.99.0 ([BREAKING], V16-10 FR-B).
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

/// Colors the terminal reports for its current theme
/// (terminal-colors.ts:10-17).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<RgbColor>,
    /// Default background (OSC 11).
    pub background: Option<RgbColor>,
    /// ANSI colors 0-15 (OSC 4). Only set when the terminal reported all 16.
    pub palette: Option<Vec<RgbColor>>,
}

/// What an OSC color reply reports: the default foreground (OSC 10),
/// background (OSC 11), or a palette index (OSC 4)
/// (terminal-colors.ts:41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscColorTarget {
    Foreground,
    Background,
    Palette(u32),
}

/// One parsed OSC color reply (terminal-colors.ts:48-58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OscColorResponse {
    pub target: OscColorTarget,
    /// `None` when the reply carried an unparseable color.
    pub rgb: Option<RgbColor>,
}

/// `hexToRgb` (terminal-colors.ts:19-25); caller guarantees six hex digits.
fn hex_to_rgb(hex: &str) -> Option<RgbColor> {
    let hex = hex.strip_prefix('#')?;
    let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
    let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
    let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
    Some(RgbColor { r, g, b })
}

/// `parseOscHexChannel` (terminal-colors.ts:27-36).
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
/// (`value.replace(/^rgba?:/i, "")`, terminal-colors.ts:66).
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

/// `parseOscColorValue` (terminal-colors.ts:60-80).
fn parse_osc_color_value(raw_value: &str) -> Option<RgbColor> {
    let value = raw_value.trim();
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

    // `rgb:`/`rgba:` responses (e.g. `rgb:0000/8000/ffff`); extra parts are
    // ignored (JS array destructuring).
    let rgb_value = strip_rgb_prefix(value);
    let mut parts = rgb_value.split('/');
    let r = parse_osc_hex_channel(parts.next()?)?;
    let g = parse_osc_hex_channel(parts.next()?)?;
    let b = parse_osc_hex_channel(parts.next()?)?;
    Some(RgbColor { r, g, b })
}

/// `parseOscColorResponse` (terminal-colors.ts:48-58). Returns `None` when
/// `data` is not an OSC 10/11/4 reply; `rgb` is `None` when it is a reply
/// with an unparseable color.
///
/// Matches the upstream regex
/// `/^\x1b\](?:(1[01])|4;(\d{1,3}));([^\x07\x1b]*)(?:\x07|\x1b\\)$/i`.
pub fn parse_osc_color_response(data: &str) -> Option<OscColorResponse> {
    let rest = data.strip_prefix("\x1b]")?;
    let (target, after) = if let Some(after) = rest.strip_prefix("10;") {
        (OscColorTarget::Foreground, after)
    } else if let Some(after) = rest.strip_prefix("11;") {
        (OscColorTarget::Background, after)
    } else {
        let after = rest.strip_prefix("4;")?;
        let semicolon = after.find(';')?;
        let index = &after[..semicolon];
        if index.is_empty() || index.len() > 3 || !index.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        (
            OscColorTarget::Palette(index.parse().ok()?),
            &after[semicolon + 1..],
        )
    };
    let value = after
        .strip_suffix('\x07')
        .or_else(|| after.strip_suffix("\x1b\\"))?;
    // `[^\x07\x1b]*`: the value must not contain a terminator char itself.
    if value.contains(['\x07', '\x1b']) {
        return None;
    }
    Some(OscColorResponse {
        target,
        rgb: parse_osc_color_value(value),
    })
}

/// `parseTerminalColorSchemeReport` (terminal-colors.ts:82-88). Matches the
/// upstream regex `/^(?:\x1b\[\?997;(1|2)n)+$/`: one or more concatenated
/// reports — terminals may batch the query reply and a change notification
/// into one read. The JS capture group keeps the LAST iteration's digit, so
/// the final report wins. The regex has no flags, so the match is
/// case-sensitive: a trailing `N` does not match.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    let mut rest = data;
    let mut last = None;
    while let Some(report) = rest.strip_prefix("\x1b[?997;") {
        if let Some(tail) = report.strip_prefix("1n") {
            last = Some(TerminalColorScheme::Dark);
            rest = tail;
        } else {
            let tail = report.strip_prefix("2n")?;
            last = Some(TerminalColorScheme::Light);
            rest = tail;
        }
    }
    // `+` requires at least one report and the whole input must match (`$`).
    if rest.is_empty() { last } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_osc_color_response_parses_osc_10_11_and_4_replies() {
        assert_eq!(
            parse_osc_color_response("\x1b]10;rgb:ffff/ffff/ffff\x07"),
            Some(OscColorResponse {
                target: OscColorTarget::Foreground,
                rgb: Some(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255
                }),
            })
        );
        assert_eq!(
            parse_osc_color_response("\x1b]4;13;#ff0080\x1b\\"),
            Some(OscColorResponse {
                target: OscColorTarget::Palette(13),
                rgb: Some(RgbColor {
                    r: 255,
                    g: 0,
                    b: 128
                }),
            })
        );
        assert_eq!(
            parse_osc_color_response("\x1b]4;1;bogus\x07"),
            Some(OscColorResponse {
                target: OscColorTarget::Palette(1),
                rgb: None,
            })
        );
        assert_eq!(parse_osc_color_response("\x1b]12;#ffffff\x07"), None);
    }

    #[test]
    fn test_parse_osc_color_response_parses_hex_and_16bit_responses() {
        assert_eq!(
            parse_osc_color_response("\x1b]11;#ffffff\x1b\\"),
            Some(OscColorResponse {
                target: OscColorTarget::Background,
                rgb: Some(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255
                }),
            })
        );
        assert_eq!(
            parse_osc_color_response("\x1b]11;rgb:0000/8000/ffff\x07"),
            Some(OscColorResponse {
                target: OscColorTarget::Background,
                rgb: Some(RgbColor {
                    r: 0,
                    g: 128,
                    b: 255
                }),
            })
        );
        assert_eq!(
            parse_osc_color_response("\x1b]11;#00008000ffff\x07"),
            Some(OscColorResponse {
                target: OscColorTarget::Background,
                rgb: Some(RgbColor {
                    r: 0,
                    g: 128,
                    b: 255
                }),
            })
        );
    }

    #[test]
    fn test_parse_osc_color_response_rejects_non_strict_responses() {
        assert_eq!(parse_osc_color_response("x\x1b]11;#ffffff\x07"), None);
        assert_eq!(parse_osc_color_response("\x1b]11;#ffffff\x07x"), None);
        assert_eq!(parse_osc_color_response("\x1b]11;#ffffff"), None);
        assert_eq!(parse_osc_color_response("\x1b]4;1234;#ffffff\x07"), None);
        assert_eq!(parse_osc_color_response("\x1b]4;;#ffffff\x07"), None);
        assert_eq!(parse_osc_color_response("\x1b]11;#ff\x07\x1b\\"), None);
    }

    #[test]
    fn test_parse_osc_color_response_accepts_case_insensitive_hex_and_rgb_prefix() {
        assert_eq!(
            parse_osc_color_response("\x1b]11;#FFAABB\x07"),
            Some(OscColorResponse {
                target: OscColorTarget::Background,
                rgb: Some(RgbColor {
                    r: 255,
                    g: 170,
                    b: 187
                }),
            })
        );
        assert_eq!(
            parse_osc_color_response("\x1b]11;RGB:ff00/ff00/ff00\x07"),
            // 4-digit channels: 0xff00 / 0xffff * 255 = 254.0088 → 254.
            Some(OscColorResponse {
                target: OscColorTarget::Background,
                rgb: Some(RgbColor {
                    r: 254,
                    g: 254,
                    b: 254
                }),
            })
        );
    }

    #[test]
    fn test_parse_osc_color_response_trims_whitespace_around_value() {
        assert_eq!(
            parse_osc_color_response("\x1b]11; #ffffff \x07"),
            Some(OscColorResponse {
                target: OscColorTarget::Background,
                rgb: Some(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255
                }),
            })
        );
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
        // Batched reports (0e633790c, terminal-colors.test.ts:118-119): the
        // last report wins.
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n\x1b[?997;2n\x1b[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?996n"), None);
        assert_eq!(parse_terminal_color_scheme_report("x\x1b[?997;1n"), None);
        // A valid report followed by trailing garbage fails the `$` anchor.
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;1nx"), None);
    }

    #[test]
    fn test_parse_terminal_color_scheme_report_rejects_uppercase_n() {
        // Upstream `/^(?:\x1b\[\?997;(1|2)n)+$/` has no flags — a trailing `N`
        // must not match.
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;1N"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;2N"), None);
    }
}
