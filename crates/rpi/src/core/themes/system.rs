//! The `system` theme: pi's colors derived from the terminal's own theme.
//!
//! Port of `packages/coding-agent/src/modes/interactive/theme/system-theme.ts`
//! @ pi v1.0.0 (a13d35a74).
//!
//! Every token belongs to a color family (its hue) and has contrast rules: it
//! must reach a contrast level on the background and on the panels it is drawn
//! on. Hue and saturation come from the terminal's palette color for the
//! family's ANSI slot, or from the family's own hue when the terminal reports
//! no palette. Lightness comes from the rules alone. Colors are built in
//! OKHSL, whose saturation is relative to the sRGB gamut, and fade toward gray
//! near black and white. A palette color never gains OKLCH chroma when it
//! moves to another lightness, so pastel palettes stay pastel.
//!
//! A contrast level is a target-lightness curve: the OKLab lightness a token
//! needs, given the lightness of the surface below it. The curves were fitted
//! to the reference theme design from the "Pi themes: system and light/dark"
//! review. On dark backgrounds they aim for nearly fixed lightness; on light
//! backgrounds the required difference grows as the background darkens.
//!
//! Depending on what the terminal reports, the theme is generated in one of
//! three tiers:
//! - background and palette: hues from the palette, lightness from the
//!   background;
//! - background only: the families' own hues, lightness from the background;
//! - nothing: ANSI palette indices and the default colors, which the terminal
//!   renders itself.
//!
//! Intentional differences from upstream:
//! - [`SystemColorValue`] owns its hex `String`, so it derives `Clone` but not
//!   `Copy` (upstream's `string | number` union is a value type in JS).
//! - [`generate_system_theme_colors`] returns the token colors as an ordered
//!   `Vec` in [`TOKEN_FAMILIES`] order instead of a JS `Record`.

use std::collections::HashMap;
use std::sync::LazyLock;

use rpi_tui::colors::{
    Color, Oklch, Rgb, color_to_hex, color_to_okhsl, color_to_oklch, color_to_rgb, okhsl_color,
    oklch_color, rgb_color,
};
use rpi_tui::oklab::{OkhslChannels, okhsl_to_rgb, oklab_to_okhsl_lightness};
use rpi_tui::terminal_colors::RgbColor;

/// [`SYSTEM_THEME_NAME`] (system-theme.ts:25).
pub const SYSTEM_THEME_NAME: &str = "system";

/// Terminal appearance: dark or light (theme.ts `ThemeAppearance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

// ============================================================================
// Recipe: color families and their tokens
// ============================================================================

/// A family's OKHSL hue and saturation: `max` at mid lightness, falling toward
/// `min` at black and white (system-theme.ts:34-42).
#[derive(Debug, Clone, Copy)]
struct Saturation {
    min: f64,
    max: f64,
}

/// A family's OKHSL hue and saturation range, plus the ANSI palette slot it
/// takes its hue and saturation from (system-theme.ts:34-42).
#[derive(Debug, Clone, Copy)]
struct Family {
    /// The family's name (the `FAMILIES` key), used to test neutrality.
    name: &'static str,
    hue: f64,
    saturation: Saturation,
    slot: u32,
}

static NEUTRAL: Family = Family {
    name: "neutral",
    hue: 231.49,
    saturation: Saturation {
        min: 0.02,
        max: 0.08,
    },
    slot: 8,
};
static BLUE: Family = Family {
    name: "blue",
    hue: 231.49,
    saturation: Saturation {
        min: 0.1,
        max: 0.68,
    },
    slot: 4,
};
static GREEN: Family = Family {
    name: "green",
    hue: 158.68,
    saturation: Saturation {
        min: 0.1,
        max: 0.76,
    },
    slot: 2,
};
static RED: Family = Family {
    name: "red",
    hue: 20.0,
    saturation: Saturation {
        min: 0.1,
        max: 0.92,
    },
    slot: 1,
};
static YELLOW: Family = Family {
    name: "yellow",
    hue: 82.36,
    saturation: Saturation { min: 0.5, max: 1.0 },
    slot: 3,
};
static ORANGE: Family = Family {
    name: "orange",
    hue: 52.0,
    saturation: Saturation {
        min: 0.12,
        max: 0.85,
    },
    slot: 3,
};
static VIOLET: Family = Family {
    name: "violet",
    hue: 295.0,
    saturation: Saturation { min: 0.2, max: 0.6 },
    slot: 5,
};
static CALAMINE: Family = Family {
    name: "calamine",
    hue: 202.43,
    saturation: Saturation {
        min: 0.1,
        max: 0.74,
    },
    slot: 6,
};
static THINKING_SLATE: Family = Family {
    name: "thinkingSlate",
    hue: 231.49,
    saturation: Saturation {
        min: 0.08,
        max: 0.2,
    },
    slot: 4,
};
static THINKING_BLUE: Family = Family {
    name: "thinkingBlue",
    hue: 231.49,
    saturation: Saturation {
        min: 0.2,
        max: 0.45,
    },
    slot: 4,
};
static THINKING_PERIWINKLE: Family = Family {
    name: "thinkingPeriwinkle",
    hue: 263.25,
    saturation: Saturation { min: 0.3, max: 0.6 },
    slot: 6,
};
static THINKING_VIOLET: Family = Family {
    name: "thinkingViolet",
    hue: 295.0,
    saturation: Saturation {
        min: 0.4,
        max: 0.75,
    },
    slot: 5,
};
static THINKING_MAGENTA: Family = Family {
    name: "thinkingMagenta",
    hue: 337.5,
    saturation: Saturation {
        min: 0.5,
        max: 0.85,
    },
    slot: 13,
};
static THINKING_RED: Family = Family {
    name: "thinkingRed",
    hue: 20.0,
    saturation: Saturation {
        min: 0.95,
        max: 1.0,
    },
    slot: 1,
};

/// `TOKEN_FAMILIES` (system-theme.ts:44-99): each token's color family, in the
/// order the generated colors are reported.
static TOKEN_FAMILIES: &[(&str, &Family)] = &[
    ("selectedBg", &BLUE),
    ("searchMatchBg", &ORANGE),
    ("userMessageBg", &BLUE),
    ("customMessageBg", &VIOLET),
    ("toolPendingBg", &NEUTRAL),
    ("toolSuccessBg", &GREEN),
    ("toolErrorBg", &RED),
    ("text", &NEUTRAL),
    ("userMessageText", &NEUTRAL),
    ("customMessageText", &NEUTRAL),
    ("toolTitle", &NEUTRAL),
    ("syntaxOperator", &NEUTRAL),
    ("syntaxPunctuation", &NEUTRAL),
    ("muted", &NEUTRAL),
    ("dim", &NEUTRAL),
    ("thinkingText", &NEUTRAL),
    ("toolOutput", &NEUTRAL),
    ("mdLinkUrl", &NEUTRAL),
    ("mdQuote", &NEUTRAL),
    ("mdQuoteBorder", &NEUTRAL),
    ("mdHr", &NEUTRAL),
    ("mdCodeBlockBorder", &NEUTRAL),
    ("toolDiffContext", &NEUTRAL),
    ("syntaxComment", &NEUTRAL),
    ("scrollbarTrack", &NEUTRAL),
    ("scrollbarThumb", &NEUTRAL),
    ("searchMatchText", &NEUTRAL),
    ("borderMuted", &NEUTRAL),
    ("accent", &VIOLET),
    ("borderAccent", &VIOLET),
    ("customMessageLabel", &VIOLET),
    ("mdCode", &VIOLET),
    ("mdListBullet", &VIOLET),
    ("syntaxType", &VIOLET),
    ("border", &BLUE),
    ("mdLink", &BLUE),
    ("syntaxKeyword", &BLUE),
    ("syntaxVariable", &CALAMINE),
    ("success", &GREEN),
    ("mdCodeBlock", &GREEN),
    ("toolDiffAdded", &GREEN),
    ("bashMode", &GREEN),
    ("syntaxNumber", &GREEN),
    ("error", &RED),
    ("toolDiffRemoved", &RED),
    ("warning", &YELLOW),
    ("mdHeading", &YELLOW),
    ("syntaxFunction", &YELLOW),
    ("syntaxString", &ORANGE),
    ("thinkingOff", &NEUTRAL),
    ("thinkingMinimal", &THINKING_SLATE),
    ("thinkingLow", &THINKING_BLUE),
    ("thinkingMedium", &THINKING_PERIWINKLE),
    ("thinkingHigh", &THINKING_VIOLET),
    ("thinkingXhigh", &THINKING_MAGENTA),
    ("thinkingMax", &THINKING_RED),
];

/// `TOKEN_SLOTS` (system-theme.ts:102): palette slots for tokens that would
/// otherwise share a hue with a similar token.
fn token_slot(token: &str) -> Option<u32> {
    match token {
        "syntaxString" => Some(2),
        "syntaxNumber" => Some(5),
        "searchMatchBg" => Some(3),
        _ => None,
    }
}

/// The token list, in `TOKEN_FAMILIES` order.
fn token_families() -> &'static [(&'static str, &'static Family)] {
    TOKEN_FAMILIES
}

/// The [`Family`] a token belongs to.
fn token_family(token: &str) -> &'static Family {
    for entry in TOKEN_FAMILIES {
        if entry.0 == token {
            return entry.1;
        }
    }
    &NEUTRAL
}

// ============================================================================
// Contrast levels and rules
// ============================================================================

/// A target-lightness curve: a polynomial in the surface's OKLab lightness
/// (coefficients in power order) and the range of surface lightness where the
/// level can be reached (system-theme.ts:110-124).
#[derive(Debug, Clone, Copy)]
struct Curve {
    coefficients: [f64; 6],
    reachable: [f64; 2],
}

/// A level's dark and light curves.
#[derive(Debug, Clone, Copy)]
struct LevelCurves {
    dark: Curve,
    light: Curve,
}

/// `LEVELS` (system-theme.ts:126-209).
static LEVELS: &[(&str, LevelCurves)] = &[
    (
        "panel",
        LevelCurves {
            dark: Curve {
                coefficients: [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
                reachable: [0.0, 0.979],
            },
            light: Curve {
                coefficients: [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
                reachable: [0.348, 1.0],
            },
        },
    ),
    (
        "track",
        LevelCurves {
            dark: Curve {
                coefficients: [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
                reachable: [0.0, 0.946],
            },
            light: Curve {
                coefficients: [
                    -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
                ],
                reachable: [0.368, 1.0],
            },
        },
    ),
    (
        "thinking0",
        LevelCurves {
            dark: Curve {
                coefficients: [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
                reachable: [0.0, 0.873],
            },
            light: Curve {
                coefficients: [
                    -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
                ],
                reachable: [0.51, 1.0],
            },
        },
    ),
    (
        "thinking1",
        LevelCurves {
            dark: Curve {
                coefficients: [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
                reachable: [0.0, 0.858],
            },
            light: Curve {
                coefficients: [
                    -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
                ],
                reachable: [0.535, 1.0],
            },
        },
    ),
    (
        "thinking2",
        LevelCurves {
            dark: Curve {
                coefficients: [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
                reachable: [0.0, 0.842],
            },
            light: Curve {
                coefficients: [
                    -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
                ],
                reachable: [0.556, 1.0],
            },
        },
    ),
    (
        "thinking3",
        LevelCurves {
            dark: Curve {
                coefficients: [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
                reachable: [0.0, 0.827],
            },
            light: Curve {
                coefficients: [
                    -72.07122,
                    445.84082,
                    -1099.57352,
                    1353.88793,
                    -829.53392,
                    202.26164,
                ],
                reachable: [0.58, 1.0],
            },
        },
    ),
    (
        "thinking4",
        LevelCurves {
            dark: Curve {
                coefficients: [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
                reachable: [0.0, 0.811],
            },
            light: Curve {
                coefficients: [
                    -110.14338,
                    674.21488,
                    -1645.75941,
                    2004.32367,
                    -1215.15899,
                    293.3183,
                ],
                reachable: [0.6, 1.0],
            },
        },
    ),
    (
        "thinking5",
        LevelCurves {
            dark: Curve {
                coefficients: [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
                reachable: [0.0, 0.795],
            },
            light: Curve {
                coefficients: [
                    -175.47701,
                    1063.54495,
                    -2570.70594,
                    3098.80776,
                    -1860.15527,
                    444.76392,
                ],
                reachable: [0.62, 1.0],
            },
        },
    ),
    (
        "thinking6",
        LevelCurves {
            dark: Curve {
                coefficients: [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
                reachable: [0.0, 0.779],
            },
            light: Curve {
                coefficients: [
                    -183.81712,
                    1094.70055,
                    -2602.68539,
                    3088.71276,
                    -1826.91131,
                    430.75931,
                ],
                reachable: [0.643, 1.0],
            },
        },
    ),
    (
        "subtle",
        LevelCurves {
            dark: Curve {
                coefficients: [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
                reachable: [0.0, 0.848],
            },
            light: Curve {
                coefficients: [
                    -232.85459,
                    1376.54473,
                    -3249.11801,
                    3827.91186,
                    -2248.29472,
                    526.55751,
                ],
                reachable: [0.657, 1.0],
            },
        },
    ),
    (
        "thumb",
        LevelCurves {
            dark: Curve {
                coefficients: [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
                reachable: [0.0, 0.823],
            },
            light: Curve {
                coefficients: [
                    -82.89897,
                    511.01355,
                    -1255.98095,
                    1540.76821,
                    -940.68087,
                    228.58523,
                ],
                reachable: [0.586, 1.0],
            },
        },
    ),
    (
        "readable",
        LevelCurves {
            dark: Curve {
                coefficients: [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
                reachable: [0.0, 0.77],
            },
            light: Curve {
                coefficients: [
                    -1554.52576,
                    8733.56817,
                    -19604.93507,
                    21977.72696,
                    -12300.99599,
                    2749.81288,
                ],
                reachable: [0.751, 1.0],
            },
        },
    ),
    (
        "emphasis",
        LevelCurves {
            dark: Curve {
                coefficients: [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
                reachable: [0.0, 0.712],
            },
            light: Curve {
                coefficients: [
                    -4948.31942,
                    26870.91986,
                    -58334.48399,
                    63280.17197,
                    -34298.01053,
                    7430.30146,
                ],
                reachable: [0.811, 1.0],
            },
        },
    ),
    (
        "textOnPanel",
        LevelCurves {
            dark: Curve {
                coefficients: [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
                reachable: [0.0, 0.542],
            },
            light: Curve {
                coefficients: [
                    -8570.89457,
                    43954.60805,
                    -90084.00702,
                    92220.6791,
                    -47152.15802,
                    9632.27113,
                ],
                reachable: [0.867, 1.0],
            },
        },
    ),
    (
        "text",
        LevelCurves {
            dark: Curve {
                coefficients: [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
                reachable: [0.0, 0.5],
            },
            light: Curve {
                coefficients: [
                    -2004.67048,
                    6664.47299,
                    -6060.70202,
                    -1792.61209,
                    5133.82359,
                    -1939.85583,
                ],
                reachable: [0.894, 1.0],
            },
        },
    ),
];

/// A rule: a token reaches `level` on each surface in `on`
/// (system-theme.ts:210-220).
#[derive(Debug, Clone, Copy)]
struct Rule {
    token: &'static str,
    on: &'static [&'static str],
    level: &'static str,
}

// Surface groups (system-theme.ts:222-244). `MESSAGE_PANELS` is inlined into
// the combined `BACKGROUND_MESSAGE`/`BACKGROUND_MESSAGE_TOOL` slices below.
const TOOL_PANELS: &[&str] = &["toolPendingBg", "toolSuccessBg", "toolErrorBg"];
#[allow(dead_code)]
const MESSAGE_PANELS: &[&str] = &["userMessageBg", "customMessageBg"];
const PANELS: &[&str] = &[
    "userMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "selectedBg",
    "searchMatchBg",
    "customMessageBg",
];
const THINKING: &[&str] = &[
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "thinkingMax",
];
const THINKING_LEVELS: &[&str] = &[
    "thinking0",
    "thinking1",
    "thinking2",
    "thinking3",
    "thinking4",
    "thinking5",
    "thinking6",
];

// The `[...spread]` surface combinations used by `RULES`.
const BACKGROUND: &[&str] = &["background"];
const BACKGROUND_SELECTED: &[&str] = &["background", "selectedBg"];
const BACKGROUND_SELECTED_TOOL: &[&str] = &[
    "background",
    "selectedBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const BACKGROUND_SELECTED_CUSTOM_TOOL: &[&str] = &[
    "background",
    "selectedBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const BACKGROUND_CUSTOM_SELECTED_TOOL: &[&str] = &[
    "background",
    "customMessageBg",
    "selectedBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const BACKGROUND_TOOL: &[&str] = &[
    "background",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const BACKGROUND_MESSAGE: &[&str] = &["background", "userMessageBg", "customMessageBg"];
const BACKGROUND_MESSAGE_TOOL: &[&str] = &[
    "background",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const CUSTOM_TOOL: &[&str] = &[
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];
const USER_MESSAGE_BG: &[&str] = &["userMessageBg"];
const SEARCH_MATCH_BG: &[&str] = &["searchMatchBg"];
const SCROLLBAR_TRACK: &[&str] = &["scrollbarTrack"];

/// Relaxation compresses levels stronger than this one toward it before
/// weakening all levels (system-theme.ts:317-318).
fn readable_floor(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => "readable",
        Appearance::Light => "subtle",
    }
}

/// Body text uses the terminal's foreground when it reaches this level, which
/// is clearly stronger than muted (system-theme.ts:321).
const FOREGROUND_LEVEL: &str = "emphasis";

/// Text-level tokens that take the terminal's foreground (system-theme.ts:324).
const FOREGROUND_TOKENS: &[&str] = &["text", "userMessageText", "toolTitle"];

/// WCAG 2 contrast ratio that body text must reach on the surfaces it is drawn
/// on (system-theme.ts:327).
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// `each` (system-theme.ts:246-247).
fn each(tokens: &[&'static str], on: &'static [&'static str], level: &'static str) -> Vec<Rule> {
    tokens
        .iter()
        .map(|&token| Rule { token, on, level })
        .collect()
}

/// Build `RULES` (system-theme.ts:249-311), preserving the upstream order.
fn build_rules() -> Vec<Rule> {
    let mut rules = Vec::new();
    rules.extend(each(PANELS, BACKGROUND, "panel"));
    rules.push(Rule {
        token: "text",
        on: BACKGROUND,
        level: "text",
    });
    rules.push(Rule {
        token: "text",
        on: BACKGROUND_SELECTED,
        level: "textOnPanel",
    });
    rules.push(Rule {
        token: "userMessageText",
        on: USER_MESSAGE_BG,
        level: "textOnPanel",
    });
    rules.push(Rule {
        token: "toolTitle",
        on: TOOL_PANELS,
        level: "textOnPanel",
    });
    rules.extend(each(
        &["accent", "success", "error", "warning"],
        BACKGROUND_SELECTED_TOOL,
        "readable",
    ));
    rules.push(Rule {
        token: "muted",
        on: BACKGROUND_SELECTED_CUSTOM_TOOL,
        level: "readable",
    });
    rules.push(Rule {
        token: "dim",
        on: BACKGROUND_SELECTED_CUSTOM_TOOL,
        level: "subtle",
    });
    rules.push(Rule {
        token: "thinkingText",
        on: BACKGROUND,
        level: "readable",
    });
    rules.push(Rule {
        token: "customMessageText",
        on: CUSTOM_TOOL,
        level: "readable",
    });
    rules.push(Rule {
        token: "customMessageLabel",
        on: BACKGROUND_CUSTOM_SELECTED_TOOL,
        level: "readable",
    });
    rules.push(Rule {
        token: "toolOutput",
        on: BACKGROUND_TOOL,
        level: "readable",
    });
    rules.extend(each(
        &[
            "mdHeading",
            "mdLink",
            "mdLinkUrl",
            "mdCode",
            "mdQuote",
            "mdCodeBlockBorder",
            "mdListBullet",
        ],
        BACKGROUND_MESSAGE,
        "readable",
    ));
    rules.push(Rule {
        token: "mdCodeBlock",
        on: BACKGROUND_MESSAGE_TOOL,
        level: "readable",
    });
    rules.extend(each(
        &["toolDiffAdded", "toolDiffRemoved", "toolDiffContext"],
        BACKGROUND_TOOL,
        "readable",
    ));
    rules.extend(each(
        &[
            "syntaxComment",
            "syntaxKeyword",
            "syntaxFunction",
            "syntaxVariable",
            "syntaxString",
            "syntaxNumber",
            "syntaxType",
            "syntaxOperator",
            "syntaxPunctuation",
        ],
        BACKGROUND_MESSAGE_TOOL,
        "readable",
    ));
    rules.push(Rule {
        token: "searchMatchText",
        on: SEARCH_MATCH_BG,
        level: "readable",
    });
    rules.extend(each(
        &["bashMode", "border", "borderAccent"],
        BACKGROUND,
        "readable",
    ));
    rules.push(Rule {
        token: "borderMuted",
        on: BACKGROUND,
        level: "subtle",
    });
    rules.extend(each(
        &["mdQuoteBorder", "mdHr"],
        BACKGROUND_MESSAGE_TOOL,
        "readable",
    ));
    rules.push(Rule {
        token: "scrollbarTrack",
        on: BACKGROUND,
        level: "track",
    });
    rules.push(Rule {
        token: "scrollbarThumb",
        on: SCROLLBAR_TRACK,
        level: "thumb",
    });
    rules.extend(THINKING.iter().enumerate().map(|(index, &token)| Rule {
        token,
        on: BACKGROUND,
        level: THINKING_LEVELS[index],
    }));
    rules
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(build_rules);

fn rules() -> &'static [Rule] {
    RULES.as_slice()
}

/// `SOLVE_ORDER` (system-theme.ts:332-344): tokens in dependency order, every
/// surface before the tokens drawn on it.
static SOLVE_ORDER: LazyLock<Vec<&'static str>> = LazyLock::new(build_solve_order);

fn build_solve_order() -> Vec<&'static str> {
    let mut order: Vec<&'static str> = Vec::new();
    for rule in rules() {
        visit(rule.token, &mut order);
    }
    order
}

fn visit(token: &'static str, order: &mut Vec<&'static str>) {
    if order.contains(&token) {
        return;
    }
    for rule in rules() {
        if rule.token != token {
            continue;
        }
        for &surface in rule.on {
            if surface != "background" {
                visit(surface, order);
            }
        }
    }
    order.push(token);
}

fn solve_order() -> &'static [&'static str] {
    SOLVE_ORDER.as_slice()
}

// ============================================================================
// Public API
// ============================================================================

/// The terminal colors the system theme is generated from
/// (system-theme.ts:359-369).
#[derive(Debug, Clone, Default)]
pub struct SystemThemeInput {
    pub foreground: Option<RgbColor>,
    pub background: Option<RgbColor>,
    /// ANSI colors 0-15.
    pub palette: Option<Vec<RgbColor>>,
    /// Saturation multiplier from 0 (grayscale) to 1. The first frame renders
    /// in grayscale until colors arrive.
    pub saturation: Option<f64>,
    /// Appearance when the terminal did not report its background, e.g. from
    /// its light/dark report or COLORFGBG.
    pub appearance_hint: Option<Appearance>,
}

/// A generated token value: a hex color, an ANSI palette index, or the
/// terminal default (`""`) (system-theme.ts:360-366).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemColorValue {
    Hex(String),
    Index(u32),
    Default,
}

/// The generated system theme (system-theme.ts:371-378).
#[derive(Debug, Clone)]
pub struct SystemThemeColors {
    /// Token name -> value, in [`TOKEN_FAMILIES`] order.
    pub colors: Vec<(&'static str, SystemColorValue)>,
    /// Foreground tokens rendered faint (SGR 2).
    pub dim: Vec<&'static str>,
    pub appearance: Option<Appearance>,
}

/// `oklabLightness` (system-theme.ts:381-383): OKLab lightness of an sRGB
/// color, 0-1.
fn oklab_lightness(color: Rgb) -> f64 {
    color_to_oklch(&Color::Rgb(color)).l
}

/// `relativeLuminance` (system-theme.ts:386-393): WCAG 2 relative luminance.
pub fn relative_luminance(color: RgbColor) -> f64 {
    relative_luminance_rgb(rgb_of(color))
}

fn relative_luminance_rgb(color: Rgb) -> f64 {
    let linear = |channel: f64| {
        let value = channel / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// `wcagContrast` (system-theme.ts:396-400): WCAG 2 contrast ratio, 1-21.
pub fn wcag_contrast(a: RgbColor, b: RgbColor) -> f64 {
    wcag_contrast_rgb(rgb_of(a), rgb_of(b))
}

fn wcag_contrast_rgb(a: Rgb, b: Rgb) -> f64 {
    let first = relative_luminance_rgb(a);
    let second = relative_luminance_rgb(b);
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// `terminalAppearance` (system-theme.ts:407-422): whether a terminal is dark
/// or light, from its reported colors.
pub fn terminal_appearance(background: RgbColor, foreground: Option<RgbColor>) -> Appearance {
    let white = Rgb {
        r: 255.0,
        g: 255.0,
        b: 255.0,
    };
    let black = Rgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
    };
    let background_rgb = rgb_of(background);
    let white_contrast = wcag_contrast_rgb(white, background_rgb);
    let black_contrast = wcag_contrast_rgb(black, background_rgb);
    if let Some(foreground) = foreground {
        let foreground_l = oklab_lightness(rgb_of(foreground));
        let background_l = oklab_lightness(background_rgb);
        if (foreground_l - background_l).abs() > 0.05 {
            let appearance = if foreground_l > background_l {
                Appearance::Dark
            } else {
                Appearance::Light
            };
            let best = if appearance == Appearance::Dark {
                white_contrast
            } else {
                black_contrast
            };
            if best >= TEXT_MINIMUM_WCAG_CONTRAST {
                return appearance;
            }
        }
    }
    if white_contrast >= black_contrast {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// A terminal color's OKHSL channels and its OKLCH chroma
/// (system-theme.ts:427-434).
#[derive(Debug, Clone, Copy)]
struct SourceColor {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(color: Rgb) -> SourceColor {
    let OkhslChannels { h, s, l } = color_to_okhsl(&Color::Rgb(color));
    SourceColor {
        h,
        s,
        l,
        chroma: color_to_oklch(&Color::Rgb(color)).c,
    }
}

/// sRGB channels (0-255) as the color system's float [`Rgb`]. `rgb_color`
/// cannot fail for `u8` channels, so the fallback is the same value.
fn rgb_of(color: RgbColor) -> Rgb {
    match rgb_color(f64::from(color.r), f64::from(color.g), f64::from(color.b)) {
        Ok(Color::Rgb(rgb)) => rgb,
        _ => Rgb {
            r: f64::from(color.r),
            g: f64::from(color.g),
            b: f64::from(color.b),
        },
    }
}

/// `okhslColor` (colors.ts): OKHSL to sRGB. Callers keep `s` and `l` in 0-1;
/// the fallback recomputes the same conversion if that ever fails.
fn okhsl_to_srgb(h: f64, s: f64, l: f64) -> Rgb {
    match okhsl_color(h, s, l) {
        Ok(color) => color_to_rgb(&color),
        Err(_) => okhsl_to_rgb(h, s, l),
    }
}

/// `oklchColor` (colors.ts): OKLCH to sRGB. Callers keep `l` in 0-1 and `c`
/// non-negative; the fallback builds the same color if that ever fails.
fn oklch_to_srgb(l: f64, c: f64, h: f64) -> Rgb {
    let color = oklch_color(l, c, h).unwrap_or(Color::Oklch(Oklch { l, c, h }));
    color_to_rgb(&color)
}

/// `bellWeight` (system-theme.ts:433-436): saturation weight at a lightness,
/// a Gaussian (center 0.5, sigma 0.25), 0 at black and white, 1 in the middle.
fn bell_weight(lightness: f64) -> f64 {
    let gaussian = |x: f64| (-((x - 0.5).powi(2)) / (2.0 * 0.25f64.powi(2))).exp();
    (gaussian(lightness) - gaussian(0.0)) / (1.0 - gaussian(0.0))
}

/// `saturationCurve` (system-theme.ts:439-442): a family's saturation curve
/// relative to its maximum.
fn saturation_curve(family: &Family, lightness: f64) -> f64 {
    let floor = if family.saturation.max > 0.0 {
        family.saturation.min / family.saturation.max
    } else {
        1.0
    };
    floor + (1.0 - floor) * bell_weight(lightness)
}

/// `levelTarget` (system-theme.ts:445-449): the target lightness for a level
/// on a surface, or `None` where the level cannot be reached.
fn level_target(level: &str, appearance: Appearance, surface_l: f64) -> Option<f64> {
    let curves = LEVELS
        .iter()
        .find(|(name, _)| *name == level)
        .map(|(_, curves)| curves)?;
    let curve = match appearance {
        Appearance::Dark => &curves.dark,
        Appearance::Light => &curves.light,
    };
    if surface_l < curve.reachable[0] || surface_l > curve.reachable[1] {
        return None;
    }
    Some(
        curve
            .coefficients
            .iter()
            .enumerate()
            .map(|(power, &coefficient)| coefficient * surface_l.powi(power as i32))
            .sum(),
    )
}

/// `anchored` (system-theme.ts:465-476): a source color's hue at another OKHSL
/// lightness, capping OKLCH chroma at the source's (with the same falloff).
fn anchored(source: SourceColor, family: &Family, lightness: f64, saturation: f64) -> Rgb {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 {
        (saturation_curve(family, lightness) / anchor).min(1.0)
    } else {
        1.0
    };
    let color = okhsl_to_srgb(source.h, source.s * falloff * saturation, lightness);
    let cap = source.chroma * falloff * saturation;
    let oklch = color_to_oklch(&Color::Rgb(color));
    if oklch.c <= cap {
        color
    } else {
        oklch_to_srgb(oklch.l, cap, source.h)
    }
}

/// `withTextContrast` (system-theme.ts:479-493): move a text color toward
/// white or black until it reaches the WCAG minimum on every surface.
fn with_text_contrast(color: Rgb, surfaces: &[Rgb], lighter: bool) -> Rgb {
    let meets = |candidate: Rgb| {
        surfaces
            .iter()
            .all(|&surface| wcag_contrast_rgb(candidate, surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let OkhslChannels { h, s, l } = color_to_okhsl(&Color::Rgb(color));
    let at = |lightness: f64| okhsl_to_srgb(h, s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    let mut low = l;
    let mut high = extreme;
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

/// The surfaces a token is drawn on, resolved to colors
/// (system-theme.ts:537-540).
fn surfaces_of(token: &str, solved: &HashMap<&'static str, Rgb>, background: Rgb) -> Vec<Rgb> {
    let mut surfaces = Vec::new();
    for rule in rules() {
        if rule.token != token {
            continue;
        }
        for &surface in rule.on {
            surfaces.push(solved.get(surface).copied().unwrap_or(background));
        }
    }
    surfaces
}

/// `generateSystemThemeColors` (system-theme.ts:452-570).
pub fn generate_system_theme_colors(input: &SystemThemeInput) -> SystemThemeColors {
    let saturation = input.saturation.unwrap_or(1.0).clamp(0.0, 1.0);
    let Some(background_color) = input.background else {
        return indexed_colors(saturation, input.appearance_hint);
    };
    let background_rgb = rgb_of(background_color);
    let foreground_rgb = input.foreground.map(rgb_of);
    let palette: Option<Vec<SourceColor>> = match input.palette.as_ref() {
        Some(colors) if colors.len() == 16 => Some(
            colors
                .iter()
                .map(|&color| source_of(rgb_of(color)))
                .collect(),
        ),
        _ => None,
    };

    let appearance = terminal_appearance(background_color, input.foreground);
    let lighter = appearance == Appearance::Dark;
    let extreme = if lighter { 1.0 } else { 0.0 };
    let background_l = oklab_lightness(background_rgb);
    let extreme_text = if lighter {
        Rgb {
            r: 255.0,
            g: 255.0,
            b: 255.0,
        }
    } else {
        Rgb {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        }
    };
    let floor_level = readable_floor(appearance);

    // A token's color at an OKLab lightness. With a palette, the palette
    // color's saturation applies at its own lightness and falls off along the
    // family's curve, never rising above it.
    let paint = |token: &str, oklab_l: f64| -> Rgb {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let family = token_family(token);
        match &palette {
            Some(colors) => {
                let slot = token_slot(token).unwrap_or(family.slot);
                anchored(colors[slot as usize], family, lightness, saturation)
            }
            None => {
                let min = family.saturation.min;
                let max = family.saturation.max;
                okhsl_to_srgb(
                    family.hue,
                    (min + (max - min) * bell_weight(lightness)) * saturation,
                    lightness,
                )
            }
        }
    };

    // The lightness a rule needs on a surface, relaxed by `t`: from 0 to 1,
    // levels stronger than the readable floor move toward it; from 1 to 2, all
    // levels move toward the surface itself.
    let target = |level: &str, surface_l: f64, t: f64| -> Option<f64> {
        let reached = level_target(level, appearance, surface_l);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let distance = reached.unwrap_or(extreme) - surface_l;
        let floor = level_target(floor_level, appearance, surface_l).unwrap_or(extreme) - surface_l;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * t.min(1.0)
        } else {
            distance
        };
        Some(surface_l + compressed * (1.0 - (t - 1.0).max(0.0)))
    };

    // Keep a panel light enough (or dark enough) that white (or black) text
    // still reaches the body text minimum on it.
    let readable =
        |color: Rgb| wcag_contrast_rgb(extreme_text, color) >= TEXT_MINIMUM_WCAG_CONTRAST;
    let limit_panel = |token: &str, l: f64| -> Rgb {
        let color = paint(token, l);
        if readable(color) {
            return color;
        }
        let mut low = background_l;
        let mut high = l;
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            if readable(paint(token, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        paint(token, low)
    };

    let solve = |t: f64| -> Option<HashMap<&'static str, Rgb>> {
        let mut colors: HashMap<&'static str, Rgb> = HashMap::new();
        colors.insert("background", background_rgb);
        for &token in solve_order() {
            let mut targets: Vec<f64> = Vec::new();
            for rule in rules() {
                if rule.token != token {
                    continue;
                }
                for &surface in rule.on {
                    let surface_color = colors.get(surface).copied().unwrap_or(background_rgb);
                    let value = target(rule.level, oklab_lightness(surface_color), t);
                    match value {
                        Some(value) if (0.0..=1.0).contains(&value) => targets.push(value),
                        _ => return None,
                    }
                }
            }
            let l = if lighter {
                targets.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                targets.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let color = if is_panel(token) {
                limit_panel(token, l)
            } else {
                paint(token, l)
            };
            colors.insert(token, color);
        }
        Some(colors)
    };

    let mut relaxation = 0.0;
    let mut colors = solve(0.0);
    if colors.is_none() {
        // Mid-gray backgrounds cannot fit every level: relax as little as
        // possible. Full relaxation always fits.
        let mut low = 0.0;
        let mut high = 2.0;
        colors = solve(high);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            match solve(middle) {
                Some(attempt) => {
                    high = middle;
                    colors = Some(attempt);
                }
                None => low = middle,
            }
        }
        relaxation = high;
    }
    let solved = colors.unwrap_or_default();

    let mut values: HashMap<&'static str, SystemColorValue> = HashMap::new();
    for &(token, _family) in token_families() {
        let value = match solved.get(token) {
            Some(color) => SystemColorValue::Hex(color_to_hex(&Color::Rgb(*color))),
            None => SystemColorValue::Default,
        };
        values.insert(token, value);
    }

    for &token in FOREGROUND_TOKENS {
        let surfaces = surfaces_of(token, &solved, background_rgb);
        // Body text uses the terminal's own foreground where it is clearly
        // stronger than muted text; otherwise the foreground's hue at just
        // enough lightness.
        let mut text = solved.get(token).copied();
        if let Some(foreground) = foreground_rgb {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|&surface| target(FOREGROUND_LEVEL, oklab_lightness(surface), relaxation))
                .collect();
            if targets
                .iter()
                .all(|target| target.is_some_and(|value| (0.0..=1.0).contains(&value)))
            {
                let needed_targets: Vec<f64> = targets.iter().flatten().copied().collect();
                let needed = if lighter {
                    needed_targets
                        .iter()
                        .copied()
                        .fold(f64::NEG_INFINITY, f64::max)
                } else {
                    needed_targets.iter().copied().fold(f64::INFINITY, f64::min)
                };
                let foreground_l = oklab_lightness(foreground);
                if (lighter && foreground_l >= needed) || (!lighter && foreground_l <= needed) {
                    values.insert(token, SystemColorValue::Default);
                    continue;
                }
                text = Some(anchored(
                    source_of(foreground),
                    &NEUTRAL,
                    oklab_to_okhsl_lightness(needed),
                    saturation,
                ));
            }
        }
        // Body text keeps at least 4.5:1 on the surfaces it is drawn on, even
        // on relaxed mid-gray backgrounds.
        if let Some(text_color) = text {
            let adjusted = with_text_contrast(text_color, &surfaces, lighter);
            values.insert(
                token,
                SystemColorValue::Hex(color_to_hex(&Color::Rgb(adjusted))),
            );
        }
    }

    let colors = token_families()
        .iter()
        .map(|&(token, _family)| {
            (
                token,
                values
                    .get(token)
                    .cloned()
                    .unwrap_or(SystemColorValue::Default),
            )
        })
        .collect();
    SystemThemeColors {
        colors,
        dim: Vec::new(),
        appearance: Some(appearance),
    }
}

/// Whether a token is a panel background (system-theme.ts:222-232).
fn is_panel(token: &str) -> bool {
    PANELS.contains(&token)
}

/// `indexedColors` (system-theme.ts:499-520): colors for terminals that
/// reported nothing. The terminal renders ANSI indices 0-15 and the default
/// colors with its own theme, so they fit any background. Neutral tokens below
/// body text are faint (SGR 2) instead of bright black. Panels have no
/// background.
fn indexed_colors(saturation: f64, appearance: Option<Appearance>) -> SystemThemeColors {
    let mut values: HashMap<&'static str, SystemColorValue> = HashMap::new();
    let mut dim: Vec<&'static str> = Vec::new();
    for &(token, family) in token_families() {
        if is_panel(token) {
            values.insert(token, SystemColorValue::Default);
            continue;
        }
        let neutral = family.name == "neutral";
        let value = if !neutral && saturation > 0.0 {
            SystemColorValue::Index(token_slot(token).unwrap_or(family.slot))
        } else {
            SystemColorValue::Default
        };
        values.insert(token, value);
        if neutral && !FOREGROUND_TOKENS.contains(&token) {
            dim.push(token);
        }
    }
    let colors = token_families()
        .iter()
        .map(|&(token, _family)| {
            (
                token,
                values
                    .get(token)
                    .cloned()
                    .unwrap_or(SystemColorValue::Default),
            )
        })
        .collect();
    SystemThemeColors {
        colors,
        dim,
        appearance,
    }
}

#[cfg(test)]
mod tests {
    use rpi_tui::colors::parse_color;

    use super::*;

    /// `rgb` (system-theme.test.ts:15): a hex string as terminal [`RgbColor`].
    fn rgb(hex: &str) -> RgbColor {
        let color = parse_color(hex).expect("valid hex color");
        let rgb = color_to_rgb(&color);
        RgbColor {
            r: rgb.r as u8,
            g: rgb.g as u8,
            b: rgb.b as u8,
        }
    }

    fn lightness(color: RgbColor) -> f64 {
        color_to_oklch(&Color::Rgb(rgb_of(color))).l
    }

    fn hue(color: RgbColor) -> f64 {
        color_to_oklch(&Color::Rgb(rgb_of(color))).h
    }

    fn chroma(color: RgbColor) -> f64 {
        color_to_oklch(&Color::Rgb(rgb_of(color))).c
    }

    fn palette(hexes: &[&str]) -> Vec<RgbColor> {
        hexes.iter().map(|hex| rgb(hex)).collect()
    }

    /// `DRACULA` (system-theme.test.ts:18-24).
    fn dracula() -> SystemThemeInput {
        SystemThemeInput {
            background: Some(rgb("#282a36")),
            foreground: Some(rgb("#f8f8f2")),
            palette: Some(palette(&[
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
                "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
                "#a4ffff", "#ffffff",
            ])),
            saturation: None,
            appearance_hint: None,
        }
    }

    fn solarized_light() -> SystemThemeInput {
        SystemThemeInput {
            background: Some(rgb("#fdf6e3")),
            foreground: Some(rgb("#657b83")),
            ..SystemThemeInput::default()
        }
    }

    fn background_only() -> SystemThemeInput {
        SystemThemeInput {
            background: Some(rgb("#1e1e1e")),
            ..SystemThemeInput::default()
        }
    }

    fn mid_gray() -> SystemThemeInput {
        SystemThemeInput {
            background: Some(rgb("#808080")),
            foreground: Some(rgb("#ffffff")),
            ..SystemThemeInput::default()
        }
    }

    /// `TERMINALS` (system-theme.test.ts:27-32).
    fn terminals() -> [(&'static str, SystemThemeInput); 4] {
        [
            ("dracula", dracula()),
            ("solarizedLight", solarized_light()),
            ("backgroundOnly", background_only()),
            ("midGray", mid_gray()),
        ]
    }

    /// The test's `PANELS` (system-theme.test.ts:34).
    const PANEL_TOKENS: &[&str] = &[
        "userMessageBg",
        "toolPendingBg",
        "toolSuccessBg",
        "toolErrorBg",
        "selectedBg",
    ];

    fn token<'a>(colors: &'a SystemThemeColors, name: &str) -> &'a SystemColorValue {
        &colors
            .colors
            .iter()
            .find(|(token, _)| *token == name)
            .expect("token is present")
            .1
    }

    /// `resolved` (system-theme.test.ts:36-40).
    fn resolved(input: &SystemThemeInput, token_name: &str) -> RgbColor {
        match token(&generate_system_theme_colors(input), token_name) {
            SystemColorValue::Default => {
                if PANEL_TOKENS.contains(&token_name) {
                    input.background.expect("background")
                } else {
                    input.foreground.expect("foreground")
                }
            }
            SystemColorValue::Hex(hex) => rgb(hex),
            SystemColorValue::Index(_) => panic!("expected a color value, got a palette index"),
        }
    }

    #[test]
    fn keeps_body_text_readable_wcag_on_the_background_and_its_panels() {
        for (name, input) in terminals() {
            let text = resolved(&input, "text");
            let background = input.background.expect("background");
            let selected_bg = resolved(&input, "selectedBg");
            for surface in [background, selected_bg] {
                assert!(wcag_contrast(text, surface) >= 4.5, "{name}");
            }
            assert!(
                wcag_contrast(
                    resolved(&input, "toolTitle"),
                    resolved(&input, "toolErrorBg")
                ) >= 4.5,
                "{name}"
            );
        }
    }

    #[test]
    fn orders_foreground_roles_by_contrast_and_keeps_panels_close_to_the_background() {
        for (name, input) in terminals() {
            let background = lightness(input.background.expect("background"));
            let offset = |token_name: &str| lightness(resolved(&input, token_name)) - background;
            // On mid-gray the levels collapse to the strongest reachable color.
            if name != "midGray" {
                assert!(
                    offset("text").abs() > offset("muted").abs(),
                    "{name} text/muted"
                );
                assert!(
                    offset("muted").abs() > offset("dim").abs(),
                    "{name} muted/dim"
                );
            }
            let lighter = generate_system_theme_colors(&input).appearance == Some(Appearance::Dark);
            for &panel in PANEL_TOKENS {
                assert!(
                    wcag_contrast(
                        resolved(&input, panel),
                        input.background.expect("background")
                    ) < 2.0,
                    "{name} {panel}"
                );
                assert_eq!(offset(panel) > 0.0, lighter, "{name} {panel}");
            }
        }
    }

    #[test]
    fn uses_the_terminal_foreground_and_palette_hues() {
        assert_eq!(
            token(&generate_system_theme_colors(&dracula()), "text"),
            &SystemColorValue::Default
        );
        // Solarized's foreground is below 4.5:1 on its own background, so text
        // is darkened.
        assert_ne!(
            token(&generate_system_theme_colors(&solarized_light()), "text"),
            &SystemColorValue::Default
        );
        let error_hue = hue(resolved(&dracula(), "error"));
        let palette_hue = hue(dracula().palette.expect("palette")[1]);
        assert!((error_hue - palette_hue).abs() < 8.0);
    }

    /// https://github.com/earendil-works/pi/issues/10255
    #[test]
    fn keeps_pastel_palette_colors_pastel_at_other_lightnesses() {
        let frappe = SystemThemeInput {
            background: Some(rgb("#303446")),
            foreground: Some(rgb("#c6d0f5")),
            palette: Some(palette(&[
                "#51576d", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be",
                "#b5bfe2", "#626880", "#e67172", "#8ec772", "#d9ba73", "#7b9ef0", "#f2a4db",
                "#5abfb5", "#a5adce",
            ])),
            saturation: None,
            appearance_hint: None,
        };
        let pink = frappe.palette.as_ref().expect("palette")[5];
        let accent = resolved(&frappe, "accent");
        // The accent is darker than the pink, but must not gain chroma (it was
        // 2x before the cap).
        assert!(lightness(accent) < lightness(pink) - 0.05);
        assert!(chroma(accent) <= chroma(pink) * 1.03);
        for panel in ["userMessageBg", "customMessageBg"] {
            assert!(chroma(resolved(&frappe, panel)) <= 0.1, "{panel}");
        }
    }

    #[test]
    fn renders_grayscale_at_zero_saturation() {
        let input = SystemThemeInput {
            saturation: Some(0.0),
            ..dracula()
        };
        let colors = generate_system_theme_colors(&input);
        let SystemColorValue::Hex(hex) = token(&colors, "error") else {
            panic!("expected a hex color");
        };
        let error = parse_color(hex).expect("valid hex color");
        assert!(color_to_oklch(&error).c < 0.005);
    }

    #[test]
    fn falls_back_to_palette_indices_and_faint_text_without_a_background() {
        let result = generate_system_theme_colors(&SystemThemeInput {
            appearance_hint: Some(Appearance::Light),
            ..SystemThemeInput::default()
        });
        assert_eq!(result.appearance, Some(Appearance::Light));
        assert_eq!(token(&result, "error"), &SystemColorValue::Index(1));
        assert_eq!(token(&result, "text"), &SystemColorValue::Default);
        assert_eq!(token(&result, "userMessageBg"), &SystemColorValue::Default);
        assert!(result.dim.contains(&"muted"));
        let zero = generate_system_theme_colors(&SystemThemeInput {
            saturation: Some(0.0),
            ..SystemThemeInput::default()
        });
        assert_eq!(token(&zero, "error"), &SystemColorValue::Default);
    }
}
