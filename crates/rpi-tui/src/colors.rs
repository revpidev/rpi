//! Color model, parsing and ANSI styling. Every color converts to sRGB, so
//! color math never fails.
//!
//! Port of `packages/tui/src/colors.ts` @ pi v1.0.0 (a13d35a74), with the
//! OKLab/OKHSL math in [`crate::oklab`].
//!
//! Intentional differences: none functional. `Rgb` channels stay `f64` (only
//! rounded when emitting hex/ANSI), and `parse_color` takes a `&str` because
//! upstream's `number` input (an ANSI index) is handled by
//! [`indexed_color`] at the call site.

use std::sync::OnceLock;

use regex::Regex;

use crate::error::TuiError;
use crate::oklab::{
    OkhslChannels, linear_srgb_to_rgb, okhsl_to_rgb, oklab_to_linear_srgb, rgb_to_okhsl,
    rgb_to_oklab,
};

/// `RgbColorValue` (colors.ts:10-15): an sRGB color, channels 0-255.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

/// `OklchColorValue` (colors.ts:17-22): an OKLCH color, `l` 0-1, `c` >= 0,
/// `h` degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

/// `Color` (colors.ts:25): a concrete color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Indexed(u8),
    Rgb(Rgb),
    Oklch(Oklch),
}

/// `TerminalColorMode` (colors.ts:26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorMode {
    Color256,
    TrueColor,
}

/// `ColorMixSpace` (colors.ts:27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMixSpace {
    Oklch,
    Srgb,
}

/// `TextAttributes` (colors.ts:35-42).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextAttributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

/// `TextStyle` (colors.ts:44-48): attributes plus an optional foreground and
/// background color.
#[derive(Debug, Clone, Default)]
pub struct TextStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub attributes: TextAttributes,
}

/// `requireFinite` (colors.ts:50-52).
fn require_finite(value: f64, name: &str) -> Result<(), TuiError> {
    if !value.is_finite() {
        return Err(TuiError::Render(format!("{name} must be finite")));
    }
    Ok(())
}

/// `indexedColor` (colors.ts:54-59): an ANSI 256-color index.
pub fn indexed_color(index: i64) -> Result<Color, TuiError> {
    if !(0..=255).contains(&index) {
        return Err(TuiError::Render(format!(
            "ANSI color index must be an integer from 0 to 255: {index}"
        )));
    }
    Ok(Color::Indexed(index as u8))
}

/// `rgbColor` (colors.ts:61-70): an sRGB color with channels 0-255.
pub fn rgb_color(r: f64, g: f64, b: f64) -> Result<Color, TuiError> {
    for (name, value) in [("r", r), ("g", g), ("b", b)] {
        require_finite(value, name)?;
        if !(0.0..=255.0).contains(&value) {
            return Err(TuiError::Render(format!(
                "{name} must be between 0 and 255: {value}"
            )));
        }
    }
    Ok(Color::Rgb(Rgb { r, g, b }))
}

/// `oklchColor` (colors.ts:72-79): an OKLCH color, hue normalized to [0, 360).
pub fn oklch_color(l: f64, c: f64, h: f64) -> Result<Color, TuiError> {
    require_finite(l, "l")?;
    require_finite(c, "c")?;
    require_finite(h, "h")?;
    if !(0.0..=1.0).contains(&l) {
        return Err(TuiError::Render(format!("l must be between 0 and 1: {l}")));
    }
    if c < 0.0 {
        return Err(TuiError::Render(format!("c must not be negative: {c}")));
    }
    Ok(Color::Oklch(Oklch {
        l,
        c,
        h: ((h % 360.0) + 360.0) % 360.0,
    }))
}

/// `NUMBER_PATTERN` (colors.ts:81): a JS-style decimal/exponent number. `\d` is
/// spelled `[0-9]` to keep the ASCII-only semantics of the upstream regex.
const NUMBER_PATTERN: &str = r"[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:e[+-]?[0-9]+)?";

/// `OKLCH_PATTERN` (colors.ts:82-85). Groups: 1 = L, 2 = "%", 3 = C, 4 = H.
fn oklch_pattern() -> String {
    format!(
        r"(?i)^oklch\(\s*({NUMBER_PATTERN})(%)?\s+({NUMBER_PATTERN})\s+({NUMBER_PATTERN})(?:deg)?\s*\)$"
    )
}

/// `OKHSL_PATTERN` (colors.ts:86-89). Groups: 1 = H, 2 = S, 3 = "%", 4 = L,
/// 5 = "%".
fn okhsl_pattern() -> String {
    format!(
        r"(?i)^okhsl\(\s*({NUMBER_PATTERN})(?:deg)?\s+({NUMBER_PATTERN})(%)?\s+({NUMBER_PATTERN})(%)?\s*\)$"
    )
}

static OKLCH_REGEX: OnceLock<Option<Regex>> = OnceLock::new();
static OKHSL_REGEX: OnceLock<Option<Regex>> = OnceLock::new();

fn match_oklch(value: &str) -> Option<regex::Captures<'_>> {
    OKLCH_REGEX
        .get_or_init(|| Regex::new(&oklch_pattern()).ok())
        .as_ref()?
        .captures(value)
}

fn match_okhsl(value: &str) -> Option<regex::Captures<'_>> {
    OKHSL_REGEX
        .get_or_init(|| Regex::new(&okhsl_pattern()).ok())
        .as_ref()?
        .captures(value)
}

fn group<'h>(captures: &regex::Captures<'h>, index: usize) -> &'h str {
    captures.get(index).map_or("", |m| m.as_str())
}

/// `Number.parseFloat` on a regex-validated number token; `NaN` on failure so
/// the downstream finite check rejects it like upstream.
fn parse_number(value: &str) -> f64 {
    value.parse::<f64>().unwrap_or(f64::NAN)
}

/// Hex branch of `parseColor` (colors.ts:104-114): `#rgb` / `#rrggbb`.
fn parse_hex_color(value: &str) -> Option<Color> {
    let digits = value.strip_prefix('#')?;
    let digits: String = match digits.len() {
        3 => digits.chars().flat_map(|digit| [digit, digit]).collect(),
        6 => digits.to_string(),
        _ => return None,
    };
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(digits.get(0..2)?, 16).ok()? as f64;
    let g = u8::from_str_radix(digits.get(2..4)?, 16).ok()? as f64;
    let b = u8::from_str_radix(digits.get(4..6)?, 16).ok()? as f64;
    rgb_color(r, g, b).ok()
}

/// `okhslColor` (colors.ts:95-106): an OKHSL color converted to sRGB.
/// Saturation is relative to the sRGB gamut at the hue and lightness, so equal
/// saturation looks equally colorful across hues and lightness.
///
/// * `h`: Hue in degrees.
/// * `s`: Saturation, 0-1.
/// * `l`: Lightness, 0-1.
pub fn okhsl_color(h: f64, s: f64, l: f64) -> Result<Color, TuiError> {
    require_finite(h, "h")?;
    require_finite(s, "s")?;
    require_finite(l, "l")?;
    if !(0.0..=1.0).contains(&s) {
        return Err(TuiError::Render(format!("s must be between 0 and 1: {s}")));
    }
    if !(0.0..=1.0).contains(&l) {
        return Err(TuiError::Render(format!("l must be between 0 and 1: {l}")));
    }
    Ok(Color::Rgb(okhsl_to_rgb(h, s, l)))
}

/// `colorToOkhsl` (colors.ts:108-110).
pub fn color_to_okhsl(color: &Color) -> OkhslChannels {
    rgb_to_okhsl(color_to_rgb(color))
}

/// `parseColor` (colors.ts:112-136), string branch only: `#rgb` / `#rrggbb` /
/// `oklch(...)` / `okhsl(...)`. Numeric ANSI indexes are handled by
/// [`indexed_color`] at the call site.
pub fn parse_color(value: &str) -> Result<Color, TuiError> {
    if let Some(color) = parse_hex_color(value) {
        return Ok(color);
    }

    if let Some(oklch) = match_oklch(value) {
        let lightness =
            parse_number(group(&oklch, 1)) / if oklch.get(2).is_some() { 100.0 } else { 1.0 };
        return oklch_color(
            lightness,
            parse_number(group(&oklch, 3)),
            parse_number(group(&oklch, 4)),
        );
    }

    if let Some(okhsl) = match_okhsl(value) {
        let saturation =
            parse_number(group(&okhsl, 2)) / if okhsl.get(3).is_some() { 100.0 } else { 1.0 };
        let lightness =
            parse_number(group(&okhsl, 4)) / if okhsl.get(5).is_some() { 100.0 } else { 1.0 };
        return okhsl_color(parse_number(group(&okhsl, 1)), saturation, lightness);
    }

    Err(TuiError::Render(format!("Invalid color value: {value}")))
}

/// `BASIC_COLORS` (colors.ts:138-155).
const BASIC_COLORS: [Rgb; 16] = [
    Rgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
    },
    Rgb {
        r: 128.0,
        g: 0.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 128.0,
        b: 0.0,
    },
    Rgb {
        r: 128.0,
        g: 128.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 0.0,
        b: 128.0,
    },
    Rgb {
        r: 128.0,
        g: 0.0,
        b: 128.0,
    },
    Rgb {
        r: 0.0,
        g: 128.0,
        b: 128.0,
    },
    Rgb {
        r: 192.0,
        g: 192.0,
        b: 192.0,
    },
    Rgb {
        r: 128.0,
        g: 128.0,
        b: 128.0,
    },
    Rgb {
        r: 255.0,
        g: 0.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 255.0,
        b: 0.0,
    },
    Rgb {
        r: 255.0,
        g: 255.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 0.0,
        b: 255.0,
    },
    Rgb {
        r: 255.0,
        g: 0.0,
        b: 255.0,
    },
    Rgb {
        r: 0.0,
        g: 255.0,
        b: 255.0,
    },
    Rgb {
        r: 255.0,
        g: 255.0,
        b: 255.0,
    },
];
/// `CUBE_VALUES` (colors.ts:156).
const CUBE_VALUES: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
/// `GRAY_VALUES` (colors.ts:157): 24 gray steps `8 + index * 10`.
const GRAY_VALUES: [f64; 24] = {
    let mut values = [0.0; 24];
    let mut index = 0;
    while index < 24 {
        values[index] = (8 + index * 10) as f64;
        index += 1;
    }
    values
};

/// `indexedToRgb` (colors.ts:159-170).
fn indexed_to_rgb(index: u8) -> Rgb {
    if index < 16 {
        return BASIC_COLORS[index as usize];
    }
    if index < 232 {
        let cube_index = usize::from(index - 16);
        return Rgb {
            r: CUBE_VALUES[cube_index / 36],
            g: CUBE_VALUES[(cube_index % 36) / 6],
            b: CUBE_VALUES[cube_index % 6],
        };
    }
    let gray = (8 + (index - 232) * 10) as f64;
    Rgb {
        r: gray,
        g: gray,
        b: gray,
    }
}

/// `isInSrgbGamut` (colors.ts:172-175).
fn is_in_srgb_gamut(linear: crate::oklab::Vector) -> bool {
    const EPSILON: f64 = 1e-7;
    linear
        .iter()
        .all(|&channel| (-EPSILON..=1.0 + EPSILON).contains(&channel))
}

/// `oklchToRgb` (colors.ts:177-199): convert OKLCH to sRGB, gamut-mapping by
/// reducing chroma with a 20-step bisection while keeping the hue fixed.
fn oklch_to_rgb(Oklch { l, c, h }: Oklch) -> Rgb {
    // Gamut mapping keeps the hue fixed, so its direction is computed once and
    // scaled by chroma.
    let radians = (h * std::f64::consts::PI) / 180.0;
    let cos = radians.cos();
    let sin = radians.sin();
    let at_chroma = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);

    let direct = at_chroma(c);
    if is_in_srgb_gamut(direct) {
        return linear_srgb_to_rgb(direct);
    }

    // Reduce chroma until the color fits. The achromatic color is always in
    // gamut, so it is the fallback when no bisection step fits, e.g.
    // `oklch(100% 0.3 150)` must map to white.
    let mut linear = at_chroma(0.0);
    let mut low = 0.0;
    let mut high = c;
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if is_in_srgb_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

/// `colorToRgb` (colors.ts:201-210).
pub fn color_to_rgb(color: &Color) -> Rgb {
    match color {
        Color::Indexed(index) => indexed_to_rgb(*index),
        Color::Rgb(rgb) => *rgb,
        Color::Oklch(oklch) => oklch_to_rgb(*oklch),
    }
}

/// `colorToOklch` (colors.ts:212-216).
pub fn color_to_oklch(color: &Color) -> Oklch {
    if let Color::Oklch(oklch) = color {
        return *oklch;
    }
    let [l, a, b] = rgb_to_oklab(color_to_rgb(color));
    Oklch {
        l,
        c: a.hypot(b),
        h: ((b.atan2(a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0,
    }
}

/// `colorToHex` (colors.ts:218-222).
pub fn color_to_hex(color: &Color) -> String {
    let rgb = color_to_rgb(color);
    let channel = |value: f64| format!("{:02x}", value.round() as i64);
    format!("#{}{}{}", channel(rgb.r), channel(rgb.g), channel(rgb.b))
}

/// `mixColors` (colors.ts:224-245): interpolate two colors in the given space.
/// `amount` is the weight of `second` (0-1).
pub fn mix_colors(
    first: &Color,
    second: &Color,
    amount: f64,
    space: ColorMixSpace,
) -> Result<Color, TuiError> {
    require_finite(amount, "amount")?;
    if !(0.0..=1.0).contains(&amount) {
        return Err(TuiError::Render(format!(
            "amount must be between 0 and 1: {amount}"
        )));
    }

    if space == ColorMixSpace::Srgb {
        let a = color_to_rgb(first);
        let b = color_to_rgb(second);
        return rgb_color(
            a.r + (b.r - a.r) * amount,
            a.g + (b.g - a.g) * amount,
            a.b + (b.b - a.b) * amount,
        );
    }

    let a = color_to_oklch(first);
    let b = color_to_oklch(second);
    let first_hue = if a.c < 1e-7 { b.h } else { a.h };
    let second_hue = if b.c < 1e-7 { first_hue } else { b.h };
    let hue_delta = ((second_hue - first_hue + 540.0) % 360.0) - 180.0;
    oklch_color(
        a.l + (b.l - a.l) * amount,
        a.c + (b.c - a.c) * amount,
        first_hue + hue_delta * amount,
    )
}

/// [`mix_colors`] with the default OKLCH interpolation space (colors.ts:224).
pub fn mix_colors_default(first: &Color, second: &Color, amount: f64) -> Result<Color, TuiError> {
    mix_colors(first, second, amount, ColorMixSpace::Oklch)
}

/// `findClosest` (colors.ts:247-259): index of the value closest to `target`.
fn find_closest(values: &[f64], target: f64) -> usize {
    let mut closest_index = 0;
    let mut closest_distance = f64::INFINITY;
    for (index, &value) in values.iter().enumerate() {
        let distance = (target - value).abs();
        if distance < closest_distance {
            closest_index = index;
            closest_distance = distance;
        }
    }
    closest_index
}

/// `colorDistance` (colors.ts:261-266): perceptually weighted squared distance.
fn color_distance(first: Rgb, second: Rgb) -> f64 {
    let dr = first.r - second.r;
    let dg = first.g - second.g;
    let db = first.b - second.b;
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// `rgbToAnsi256` (colors.ts:268-285): nearest 256-color palette entry.
fn rgb_to_ansi256(color: Rgb) -> usize {
    let r_index = find_closest(&CUBE_VALUES, color.r);
    let g_index = find_closest(&CUBE_VALUES, color.g);
    let b_index = find_closest(&CUBE_VALUES, color.b);
    let cube_color = Rgb {
        r: CUBE_VALUES[r_index],
        g: CUBE_VALUES[g_index],
        b: CUBE_VALUES[b_index],
    };
    let cube_index = 16 + 36 * r_index + 6 * g_index + b_index;

    let gray = (0.299 * color.r + 0.587 * color.g + 0.114 * color.b).round();
    let gray_offset = find_closest(&GRAY_VALUES, gray);
    let gray_value = GRAY_VALUES[gray_offset];
    let spread = color.r.max(color.g).max(color.b) - color.r.min(color.g).min(color.b);
    if spread < 10.0
        && color_distance(
            color,
            Rgb {
                r: gray_value,
                g: gray_value,
                b: gray_value,
            },
        ) < color_distance(color, cube_color)
    {
        return 232 + gray_offset;
    }
    cube_index
}

/// `colorAnsi` (colors.ts:287-295).
fn color_ansi(color: &Color, mode: TerminalColorMode, background: bool) -> String {
    let code = if background { 48 } else { 38 };
    if let Color::Indexed(index) = color {
        return format!("\x1b[{code};5;{index}m");
    }

    let rgb = color_to_rgb(color);
    if mode == TerminalColorMode::TrueColor {
        return format!(
            "\x1b[{code};2;{};{};{}m",
            rgb.r.round() as i64,
            rgb.g.round() as i64,
            rgb.b.round() as i64
        );
    }
    format!("\x1b[{code};5;{}m", rgb_to_ansi256(rgb))
}

/// `foregroundAnsi` (colors.ts:297-299).
pub fn foreground_ansi(color: &Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, false)
}

/// `backgroundAnsi` (colors.ts:301-303).
pub fn background_ansi(color: &Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, true)
}

/// `styleText` (colors.ts:305-312).
pub fn style_text(text: &str, options: &TextStyle, mode: TerminalColorMode) -> String {
    let fg = options
        .fg
        .as_ref()
        .map(|color| foreground_ansi(color, mode));
    let bg = options
        .bg
        .as_ref()
        .map(|color| background_ansi(color, mode));
    style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), &options.attributes)
}

/// `styleTextWithAnsi` (colors.ts:314-348): like [`style_text`], but with
/// precomputed color escape sequences, e.g. cached theme colors. Colors in
/// `options` are ignored.
pub fn style_text_with_ansi(
    text: &str,
    fg_ansi: Option<&str>,
    bg_ansi: Option<&str>,
    attributes: &TextAttributes,
) -> String {
    // Upstream `if (fgAnsi)`: an empty sequence is falsy, so treat it as unset.
    let fg_ansi = fg_ansi.filter(|value| !value.is_empty());
    let bg_ansi = bg_ansi.filter(|value| !value.is_empty());

    // Resets are prepended so they close in reverse order of the opening
    // sequences.
    let mut prefix = String::new();
    let mut suffix = String::new();
    if let Some(fg) = fg_ansi {
        prefix.push_str(fg);
        suffix = "\x1b[39m".to_string();
    }
    if let Some(bg) = bg_ansi {
        prefix.push_str(bg);
        suffix = format!("\x1b[49m{suffix}");
    }
    if attributes.bold {
        prefix.push_str("\x1b[1m");
    }
    if attributes.dim {
        prefix.push_str("\x1b[2m");
    }
    if attributes.bold || attributes.dim {
        suffix = format!("\x1b[22m{suffix}");
    }
    if attributes.italic {
        prefix.push_str("\x1b[3m");
        suffix = format!("\x1b[23m{suffix}");
    }
    if attributes.underline {
        prefix.push_str("\x1b[4m");
        suffix = format!("\x1b[24m{suffix}");
    }
    if attributes.inverse {
        prefix.push_str("\x1b[7m");
        suffix = format!("\x1b[27m{suffix}");
    }
    if attributes.strikethrough {
        prefix.push_str("\x1b[9m");
        suffix = format!("\x1b[29m{suffix}");
    }
    format!("{prefix}{text}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_oklch_colors_and_rejects_everything_else() {
        assert_eq!(
            parse_color("#abc").unwrap(),
            Color::Rgb(Rgb {
                r: 170.0,
                g: 187.0,
                b: 204.0
            })
        );
        assert_eq!(
            parse_color("oklch(62% 0.1 200)").unwrap(),
            Color::Oklch(Oklch {
                l: 0.62,
                c: 0.1,
                h: 200.0
            })
        );
        assert!(matches!(parse_color(""), Err(TuiError::Render(_))));
        assert!(matches!(parse_color("red"), Err(TuiError::Render(_))));
    }

    #[test]
    fn gamut_maps_oklch_to_srgb_including_the_lightness_limits() {
        assert_eq!(
            color_to_rgb(&oklch_color(0.627955, 0.257683, 29.2339).unwrap()),
            Rgb {
                r: 255.0,
                g: 0.0,
                b: 0.0
            }
        );
        assert_eq!(
            color_to_rgb(&oklch_color(1.0, 0.3, 150.0).unwrap()),
            Rgb {
                r: 255.0,
                g: 255.0,
                b: 255.0
            }
        );
        assert_eq!(
            color_to_rgb(&oklch_color(0.0, 0.3, 150.0).unwrap()),
            Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0
            }
        );
    }

    #[test]
    fn parses_okhsl_colors_and_round_trips_them() {
        // Full saturation at the red cusp is pure sRGB red.
        assert_eq!(
            parse_color("okhsl(29.23 100% 56.8%)").unwrap(),
            Color::Rgb(Rgb {
                r: 255.0,
                g: 0.0,
                b: 0.0
            })
        );
        assert_eq!(
            parse_color("OKHSL(250deg 60% 55%)").unwrap(),
            okhsl_color(250.0, 0.6, 0.55).unwrap()
        );
        match parse_color("okhsl(250 160% 55%)") {
            Err(TuiError::Render(message)) => {
                assert!(message.contains("s must be between 0 and 1"), "{message}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
        for hex in ["#4f8eb3", "#20242a", "#f8f9fa"] {
            let color = parse_color(hex).unwrap();
            let channels = color_to_okhsl(&color);
            let round_tripped = okhsl_color(channels.h, channels.s, channels.l).unwrap();
            assert_eq!(color_to_hex(&round_tripped), hex);
        }
    }

    #[test]
    fn styles_text_and_closes_sequences_in_reverse_order() {
        let style = TextStyle {
            fg: Some(rgb_color(18.0, 52.0, 86.0).unwrap()),
            bg: Some(indexed_color(9).unwrap()),
            attributes: TextAttributes {
                bold: true,
                italic: true,
                ..TextAttributes::default()
            },
        };
        assert_eq!(
            style_text("Ready", &style, TerminalColorMode::TrueColor),
            "\x1b[38;2;18;52;86m\x1b[48;5;9m\x1b[1m\x1b[3mReady\x1b[23m\x1b[22m\x1b[49m\x1b[39m"
        );

        let fg_only = TextStyle {
            fg: Some(rgb_color(18.0, 52.0, 86.0).unwrap()),
            ..TextStyle::default()
        };
        let styled = style_text("Ready", &fg_only, TerminalColorMode::Color256);
        assert!(styled.starts_with("\x1b[38;5;") && styled.ends_with("Ready\x1b[39m"));
    }
}
