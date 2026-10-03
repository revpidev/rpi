//! Oklab and OKHSL <-> sRGB conversion. `colors.rs` builds its OKLCH, OKHSL,
//! and color mixing on it.
//!
//! Port of `packages/tui/src/oklab.ts` @ pi v1.0.0 (a13d35a74).
//!
//! Oklab and OKHSL are Björn Ottosson's color spaces; OKHSL's saturation is
//! relative to the sRGB gamut at each hue and lightness. This is a port of his
//! reference implementation
//! (<https://bottosson.github.io/posts/colorpicker/>),
//! Copyright (c) 2021 Björn Ottosson, used under the MIT license:
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions: The above copyright
//! notice and this permission notice shall be included in all copies or
//! substantial portions of the Software. THE SOFTWARE IS PROVIDED "AS IS",
//! WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED.
//!
//! Intentional differences: none. All math uses `f64` throughout; the public
//! functions are pure and allocate nothing.

use crate::colors::Rgb;

/// A 3-component vector (oklab.ts:15).
pub type Vector = [f64; 3];
/// A 3x3 matrix (oklab.ts:16).
type Matrix = [[f64; 3]; 3];

/// `multiply` (oklab.ts:18-19): matrix-vector product.
fn multiply(matrix: Matrix, [x, y, z]: Vector) -> Vector {
    std::array::from_fn(|row| matrix[row][0] * x + matrix[row][1] * y + matrix[row][2] * z)
}

// ============================================================================
// OKHSL <-> sRGB
// ============================================================================

/// `LINEAR_SRGB_TO_LMS` (oklab.ts:25-29).
const LINEAR_SRGB_TO_LMS: Matrix = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
/// `LMS_TO_LAB` (oklab.ts:30-34).
const LMS_TO_LAB: Matrix = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
/// `LAB_TO_LMS` (oklab.ts:35-39).
const LAB_TO_LMS: Matrix = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
/// `LMS_TO_LINEAR_SRGB` (oklab.ts:40-44).
const LMS_TO_LINEAR_SRGB: Matrix = [
    [4.076741636075958, -3.307711539258063, 0.2309699031821043],
    [-1.2684379732850315, 2.609757349287688, -0.341319376002657],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];

/// `SATURATION_FIT` (oklab.ts:49-63): per sRGB channel (red, green, blue) the
/// (a, b) half-plane where that channel clips first, and the polynomial
/// approximating the maximum saturation there.
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
    (
        [-1.8817031, -0.80936501],
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        [1.8144408, -1.19445267],
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        [0.13110758, 1.81333971],
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];

const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

/// `oklabToOkhslLightness` (oklab.ts:70-71): Oklab lightness to OKHSL lightness.
pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powi(2) + 4.0 * K2 * K3 * x).sqrt())
}

/// `okhslToOklabLightness` (oklab.ts:73): OKHSL lightness to Oklab lightness.
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

/// `linearToSrgb` (oklab.ts:76-77): linear to encoded channel, both 0-1.
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.0031308 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// `srgbToLinear` (oklab.ts:79): encoded to linear channel, both 0-1.
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// `oklabToLinearSrgb` (oklab.ts:82-84): Oklab [L, a, b] to linear sRGB
/// [r, g, b] (0-1, may leave the gamut).
pub fn oklab_to_linear_srgb(lab: Vector) -> Vector {
    multiply(
        LMS_TO_LINEAR_SRGB,
        multiply(LAB_TO_LMS, lab).map(|value| value.powi(3)),
    )
}

/// `linearSrgbToOklab` (oklab.ts:87-89): linear sRGB [r, g, b] (0-1) to
/// Oklab [L, a, b].
fn linear_srgb_to_oklab(rgb: Vector) -> Vector {
    multiply(LMS_TO_LAB, multiply(LINEAR_SRGB_TO_LMS, rgb).map(f64::cbrt))
}

/// `rgbToOklab` (oklab.ts:92-94): sRGB channels (0-255) to Oklab [L, a, b].
pub fn rgb_to_oklab(rgb: Rgb) -> Vector {
    linear_srgb_to_oklab([
        srgb_to_linear(rgb.r / 255.0),
        srgb_to_linear(rgb.g / 255.0),
        srgb_to_linear(rgb.b / 255.0),
    ])
}

/// `linearSrgbToRgb` (oklab.ts:97-100): linear sRGB [r, g, b] to sRGB channels
/// (0-255, rounded), clipping out-of-gamut channels.
pub fn linear_srgb_to_rgb(linear: Vector) -> Rgb {
    let channel = |value: f64| linear_to_srgb(value).clamp(0.0, 1.0) * 255.0;
    Rgb {
        r: channel(linear[0]).round(),
        g: channel(linear[1]).round(),
        b: channel(linear[2]).round(),
    }
}

/// `lmsSlopes` (oklab.ts:103-105): rate of change of each cube-root LMS
/// component along a chroma direction (a, b).
fn lms_slopes(a: f64, b: f64) -> Vector {
    std::array::from_fn(|row| LAB_TO_LMS[row][1] * a + LAB_TO_LMS[row][2] * b)
}

/// `maxSaturation` (oklab.ts:108-121): largest saturation (C/L) inside sRGB for
/// hue (a, b): polynomial fit plus one Halley step.
fn max_saturation(a: f64, b: f64) -> f64 {
    let mut channel = SATURATION_FIT.len() - 1;
    for (index, (plane, _)) in SATURATION_FIT.iter().enumerate() {
        if index == 2 || plane[0] * a + plane[1] * b > 1.0 {
            channel = index;
            break;
        }
    }
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

    let slopes = lms_slopes(a, b);
    let base = std::array::from_fn(|i| 1.0 + saturation * slopes[i]);
    let dot =
        |values: [f64; 3]| weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2];
    let f = dot(base.map(|value| value.powi(3)));
    let f1 = dot(std::array::from_fn(|i| 3.0 * slopes[i] * base[i] * base[i]));
    let f2 = dot(std::array::from_fn(|i| {
        6.0 * slopes[i] * slopes[i] * base[i]
    }));
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// `cusp` (oklab.ts:124-128): Oklab lightness and chroma of the most saturated
/// sRGB color of hue (a, b).
fn cusp(a: f64, b: f64) -> [f64; 2] {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let lightness = (1.0 / linear[0].max(linear[1]).max(linear[2])).cbrt();
    [lightness, lightness * saturation]
}

/// `maxChroma` (oklab.ts:131-149): chroma where the constant-lightness line at
/// `lightness` leaves the sRGB gamut.
fn max_chroma(a: f64, b: f64, lightness: f64, [cusp_l, cusp_c]: [f64; 2]) -> f64 {
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    // Upper half: triangle edge, then one Halley step against each channel
    // reaching 1.
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = std::array::from_fn(|i| lightness + t * slopes[i]);
    let cubes = lms.map(|value| value.powi(3));
    let first = std::array::from_fn(|i| 3.0 * slopes[i] * lms[i] * lms[i]);
    let second = std::array::from_fn(|i| 6.0 * slopes[i] * slopes[i] * lms[i]);
    let dot = |row: [f64; 3], values: [f64; 3]| {
        row[0] * values[0] + row[1] * values[1] + row[2] * values[2]
    };
    let mut min_step = f64::MAX;
    for row in LMS_TO_LINEAR_SRGB {
        let f = dot(row, cubes) - 1.0;
        let f1 = dot(row, first);
        let f2 = dot(row, second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        let step = if u >= 0.0 { -f * u } else { f64::MAX };
        if step < min_step {
            min_step = step;
        }
    }
    t + min_step
}

/// `chromaStops` (oklab.ts:152-177): OKHSL's chroma reference points at
/// lightness L and hue (a, b): [c0, cMid, cMax].
fn chroma_stops(l: f64, a: f64, b: f64) -> [f64; 3] {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max / (l * (peak[1] / peak[0])).min((1.0 - l) * (peak[1] / (1.0 - peak[0])));
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / ((1.0 / (l * mid_s)).powi(4) + (1.0 / ((1.0 - l) * mid_t)).powi(4)))
            .sqrt()
            .sqrt();
    let c0 = (1.0 / ((1.0 / (l * 0.4)).powi(2) + (1.0 / ((1.0 - l) * 0.8)).powi(2))).sqrt();
    [c0, c_mid, c_max]
}

/// `okhslToRgb` (oklab.ts:185-207): Convert OKHSL to sRGB channels (0-255,
/// rounded), clipping out-of-gamut channels.
///
/// * `hue`: Hue in degrees.
/// * `saturation`: Saturation, 0-1.
/// * `lightness`: Lightness, 0-1.
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab: Vector = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = (2.0 * std::f64::consts::PI * (((hue % 360.0) + 360.0) % 360.0)) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let [c0, c_mid, c_max] = chroma_stops(l, a, b);
        // Chroma rises from 0 through cMid at s = 0.8 to cMax at s = 1.
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid * c_mid * 1.25f64.powi(2)) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// OKHSL channels: hue in degrees, saturation and lightness 0-1 (oklab.ts:
/// return type of `rgbToOkhsl`; colors.ts:21-25).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OkhslChannels {
    pub h: f64,
    pub s: f64,
    pub l: f64,
}

/// `rgbToOkhsl` (oklab.ts:213-236): Convert sRGB channels (0-255) to OKHSL.
///
/// Returns hue `h` in degrees (0 for grays), saturation `s` and lightness `l`
/// 0-1.
pub fn rgb_to_okhsl(rgb: Rgb) -> OkhslChannels {
    let [l_channel, lab_a, lab_b] = rgb_to_oklab(rgb);
    let chroma = lab_a.hypot(lab_b);
    let lightness = oklab_to_okhsl_lightness(l_channel);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return OkhslChannels {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }

    let hue = ((lab_b.atan2(lab_a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0;
    let [c0, c_mid, c_max] = chroma_stops(l_channel, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid * c_mid * 1.25f64.powi(2)) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    OkhslChannels {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_red_to_oklab_and_back() {
        let lab = rgb_to_oklab(Rgb {
            r: 255.0,
            g: 0.0,
            b: 0.0,
        });
        // Known Oklab coordinates for sRGB red.
        assert!((lab[0] - 0.627955).abs() < 1e-4, "L={}", lab[0]);
        assert!((lab[1] - 0.224863).abs() < 1e-4, "a={}", lab[1]);
        assert!((lab[2] - 0.125846).abs() < 1e-4, "b={}", lab[2]);
        assert_eq!(
            linear_srgb_to_rgb(oklab_to_linear_srgb(lab)),
            Rgb {
                r: 255.0,
                g: 0.0,
                b: 0.0
            }
        );
    }

    #[test]
    fn red_sits_at_the_okhsl_cusp_with_full_saturation() {
        let channels = rgb_to_okhsl(Rgb {
            r: 255.0,
            g: 0.0,
            b: 0.0,
        });
        assert!((channels.s - 1.0).abs() < 1e-6, "s={}", channels.s);
        assert!((channels.h - 29.2339).abs() < 1e-2, "h={}", channels.h);
        // Tolerance absorbs f64 evaluation-order differences: `channels.l`
        // comes from the computed Oklab lightness while the right side uses
        // the rounded constant 0.627955. Upstream asserts closeness, not bit
        // equality; the observed gap is ~4e-7.
        assert!(
            (channels.l - oklab_to_okhsl_lightness(0.627955)).abs() < 1e-5,
            "l={}",
            channels.l
        );
    }

    #[test]
    fn grays_have_zero_saturation_and_hue() {
        let channels = rgb_to_okhsl(Rgb {
            r: 128.0,
            g: 128.0,
            b: 128.0,
        });
        assert_eq!(channels.h, 0.0);
        assert_eq!(channels.s, 0.0);
    }
}
