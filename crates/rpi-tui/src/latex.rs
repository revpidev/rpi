//! Port of `packages/tui/src/latex.ts` @ 4181f66 (pi v0.84.1+; upstream
//! commits 05e89b418 "feat(tui): render LaTeX math in Markdown" and
//! aa601d7ba "fix(tui): correct LaTeX whitespace and matrix layouts", both
//! already merged in the pinned file).
//!
//! Terminal-friendly Unicode-math renderer for basic LaTeX math input.
//! Key structure mapping:
//! - `renderLatex(source, { display })` -> `render_latex(source, display)`
//! - `normalizeOutput` -> `normalize_output`
//! - `formatScript` / `formatFraction` / `formatRoot` -> same snake_case
//! - `FractionNode` / `OperatorNode` / `MatrixNode` -> `LayoutNode` enum
//! - `Layout`, `padLayoutLine`, `joinLayouts`, `renderLayout` -> same names
//! - `LatexParser` -> `LatexParser`
//! - `visibleWidth` (utils.ts) -> `crate::utils::visible_width`
//! - the PUA markers `\u{f0000}`-`\u{f0005}` are layout markers, named-operator
//!   markers and protected spaces; they are resolved/cleared before the result
//!   is returned (upstream replaces `\u{f0002}` with " " at the very end).
//!
//! Intentional differences:
//! - Pure std implementation; upstream's handful of regexes are hand-rolled
//!   character scanners with identical semantics (see the inline comments).
//! - `[\p{L}\p{N}]` (ECMA-262 Unicode property escapes) is implemented as Rust
//!   `char::is_alphanumeric()` (`Alphabetic || Numeric`). This matches
//!   `[\p{L}\p{N}]` except for characters that are `Alphabetic=Yes` outside
//!   General Category L (e.g. U+0345); such characters do not occur in the
//!   ported test corpus.
//! - ECMA-262 `\s` and `String.trim()` semantics (U+FEFF is whitespace,
//!   U+0085 is not) are implemented locally (`is_ecma_space` / `trim_ecma*`)
//!   instead of Rust's Unicode White_Space.
//! - Source text is walked as Rust `char` (Unicode scalar values) instead of
//!   UTF-16 code units. Identical output for all-ASCII sources (the whole
//!   upstream test corpus); the only divergence is raw astral characters in
//!   the source, which upstream would split into surrogate halves.
//! - `read_raw_group`'s `\` skip advances past the whole next scalar value
//!   instead of one UTF-16 code unit (same result for ASCII sources).
//! - `render_latex` takes a `bool` instead of the upstream options object.
//!
//! Ported tests: `packages/tui/test/latex.test.ts` (all defineCases tables
//! and standalone `it` cases) in `#[cfg(test)] mod tests` below.

use crate::utils::visible_width;

// Symbol and command tables (extracted 1:1 from latex.ts, order preserved).
const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ϵ"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("varkappa", "ϰ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "ϕ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    ("pm", "±"),
    ("mp", "∓"),
    ("times", "×"),
    ("div", "÷"),
    ("cdot", "·"),
    ("ast", "∗"),
    ("star", "⋆"),
    ("circ", "∘"),
    ("bullet", "•"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("oslash", "⊘"),
    ("odot", "⊙"),
    ("bigcirc", "○"),
    ("dagger", "†"),
    ("ddagger", "‡"),
    ("amalg", "⨿"),
    ("uplus", "⊎"),
    ("sqcap", "⊓"),
    ("sqcup", "⊔"),
    ("triangleleft", "◁"),
    ("triangleright", "▷"),
    ("wr", "≀"),
    ("cap", "∩"),
    ("cup", "∪"),
    ("bigcap", "⋂"),
    ("bigcup", "⋃"),
    ("bigwedge", "⋀"),
    ("bigvee", "⋁"),
    ("bigsqcup", "⨆"),
    ("biguplus", "⨄"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigodot", "⨀"),
    ("setminus", "∖"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("supset", "⊃"),
    ("subseteq", "⊆"),
    ("supseteq", "⊇"),
    ("sqsubset", "⊏"),
    ("sqsupset", "⊐"),
    ("sqsubseteq", "⊑"),
    ("sqsupseteq", "⊒"),
    ("prec", "≺"),
    ("preceq", "≼"),
    ("succ", "≻"),
    ("succeq", "≽"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("le", "≤"),
    ("leq", "≤"),
    ("leqslant", "≤"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("geqslant", "≥"),
    ("ne", "≠"),
    ("neq", "≠"),
    ("equiv", "≡"),
    ("approx", "≈"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("cong", "≅"),
    ("asymp", "≍"),
    ("doteq", "≐"),
    ("propto", "∝"),
    ("parallel", "∥"),
    ("perp", "⊥"),
    ("mid", "∣"),
    ("vdash", "⊢"),
    ("dashv", "⊣"),
    ("models", "⊨"),
    ("Vdash", "⊩"),
    ("Vvdash", "⊪"),
    ("nvdash", "⊬"),
    ("nvDash", "⊭"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("neg", "¬"),
    ("land", "∧"),
    ("wedge", "∧"),
    ("lor", "∨"),
    ("vee", "∨"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("longrightarrow", "→"),
    ("leftarrow", "←"),
    ("longleftarrow", "←"),
    ("gets", "←"),
    ("leftrightarrow", "↔"),
    ("longleftrightarrow", "↔"),
    ("hookleftarrow", "↩"),
    ("hookrightarrow", "↪"),
    ("twoheadleftarrow", "↞"),
    ("twoheadrightarrow", "↠"),
    ("leftharpoonup", "↼"),
    ("leftharpoondown", "↽"),
    ("rightharpoonup", "⇀"),
    ("rightharpoondown", "⇁"),
    ("rightleftharpoons", "⇌"),
    ("leftrightharpoons", "⇋"),
    ("nearrow", "↗"),
    ("searrow", "↘"),
    ("swarrow", "↙"),
    ("nwarrow", "↖"),
    ("rightsquigarrow", "⇝"),
    ("leadsto", "⇝"),
    ("Rightarrow", "⇒"),
    ("Longrightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Longleftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("Longleftrightarrow", "⇔"),
    ("implies", "⇒"),
    ("iff", "⇔"),
    ("mapsto", "↦"),
    ("longmapsto", "↦"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("coprod", "∐"),
    ("infty", "∞"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("angle", "∠"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("aleph", "ℵ"),
    ("beth", "ℶ"),
    ("gimel", "ℷ"),
    ("daleth", "ℸ"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("triangle", "△"),
    ("square", "□"),
    ("lozenge", "◊"),
    ("checkmark", "✓"),
    ("complement", "∁"),
    ("wp", "℘"),
    ("prime", "′"),
    ("ldots", "…"),
    ("dots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("ell", "ℓ"),
    ("hbar", "ℏ"),
    ("Im", "ℑ"),
    ("Re", "ℜ"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("vert", "|"),
    ("lvert", "|"),
    ("rvert", "|"),
    ("Vert", "‖"),
    ("lVert", "‖"),
    ("rVert", "‖"),
    ("lbrace", "{"),
    ("rbrace", "}"),
    ("backslash", "\\\\"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("colon", ":"),
];

const NEGATED_SYMBOLS: &[(char, &str)] = &[
    ('<', "≮"),
    ('>', "≯"),
    ('=', "≠"),
    ('∈', "∉"),
    ('∋', "∌"),
    ('∣', "∤"),
    ('∥', "∦"),
    ('∼', "≁"),
    ('≃', "≄"),
    ('≅', "≇"),
    ('≈', "≉"),
    ('≡', "≢"),
    ('≤', "≰"),
    ('≥', "≱"),
    ('≺', "⊀"),
    ('≻', "⊁"),
    ('⊂', "⊄"),
    ('⊃', "⊅"),
    ('⊆', "⊈"),
    ('⊇', "⊉"),
    ('⊢', "⊬"),
    ('⊨', "⊭"),
    ('↔', "↮"),
    ('←', "↚"),
    ('→', "↛"),
    ('⇒', "⇏"),
    ('⇐', "⇍"),
    ('⇔', "⇎"),
    ('≼', "⋠"),
    ('≽', "⋡"),
];

const BLACKBOARD: &[(char, &str)] = &[
    ('C', "ℂ"),
    ('H', "ℍ"),
    ('N', "ℕ"),
    ('P', "ℙ"),
    ('Q', "ℚ"),
    ('R', "ℝ"),
    ('Z', "ℤ"),
];

const SUPERSCRIPTS: &[(char, &str)] = &[
    ('0', "⁰"),
    ('1', "¹"),
    ('2', "²"),
    ('3', "³"),
    ('4', "⁴"),
    ('5', "⁵"),
    ('6', "⁶"),
    ('7', "⁷"),
    ('8', "⁸"),
    ('9', "⁹"),
    ('+', "⁺"),
    ('-', "⁻"),
    ('=', "⁼"),
    ('(', "⁽"),
    (')', "⁾"),
    ('a', "ᵃ"),
    ('b', "ᵇ"),
    ('c', "ᶜ"),
    ('d', "ᵈ"),
    ('e', "ᵉ"),
    ('f', "ᶠ"),
    ('g', "ᵍ"),
    ('h', "ʰ"),
    ('i', "ⁱ"),
    ('j', "ʲ"),
    ('k', "ᵏ"),
    ('l', "ˡ"),
    ('m', "ᵐ"),
    ('n', "ⁿ"),
    ('o', "ᵒ"),
    ('p', "ᵖ"),
    ('r', "ʳ"),
    ('s', "ˢ"),
    ('t', "ᵗ"),
    ('u', "ᵘ"),
    ('v', "ᵛ"),
    ('w', "ʷ"),
    ('x', "ˣ"),
    ('y', "ʸ"),
    ('z', "ᶻ"),
];

const SUBSCRIPTS: &[(char, &str)] = &[
    ('0', "₀"),
    ('1', "₁"),
    ('2', "₂"),
    ('3', "₃"),
    ('4', "₄"),
    ('5', "₅"),
    ('6', "₆"),
    ('7', "₇"),
    ('8', "₈"),
    ('9', "₉"),
    ('+', "₊"),
    ('-', "₋"),
    ('=', "₌"),
    ('(', "₍"),
    (')', "₎"),
    ('a', "ₐ"),
    ('e', "ₑ"),
    ('h', "ₕ"),
    ('i', "ᵢ"),
    ('j', "ⱼ"),
    ('k', "ₖ"),
    ('l', "ₗ"),
    ('m', "ₘ"),
    ('n', "ₙ"),
    ('o', "ₒ"),
    ('p', "ₚ"),
    ('r', "ᵣ"),
    ('s', "ₛ"),
    ('t', "ₜ"),
    ('u', "ᵤ"),
    ('v', "ᵥ"),
    ('x', "ₓ"),
];

const ACCENTS: &[(&str, &str)] = &[
    ("acute", "́"),
    ("bar", "̅"),
    ("breve", "̆"),
    ("check", "̌"),
    ("ddot", "̈"),
    ("dot", "̇"),
    ("grave", "̀"),
    ("hat", "̂"),
    ("mathring", "̊"),
    ("overleftarrow", "⃖"),
    ("overleftrightarrow", "⃡"),
    ("overline", "̅"),
    ("overrightarrow", "⃗"),
    ("tilde", "̃"),
    ("underline", "̲"),
    ("vec", "⃗"),
    ("widehat", "̂"),
    ("widetilde", "̃"),
];

const NAMED_OPERATORS: &[&str] = &[
    "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim",
    "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min",
    "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
];

const LIMIT_OPERATORS: &[&str] = &[
    "argmax", "argmin", "inf", "injlim", "lim", "liminf", "limsup", "max", "min", "projlim", "sup",
];

const DISPLAY_LIMIT_SYMBOLS: &[&str] = &[
    "bigcap",
    "bigcup",
    "bigodot",
    "bigoplus",
    "bigotimes",
    "bigsqcup",
    "biguplus",
    "bigvee",
    "bigwedge",
    "coprod",
    "int",
    "iint",
    "iiint",
    "oint",
    "prod",
    "sum",
];

const RELATION_COMMANDS: &[&str] = &[
    "Leftarrow",
    "Leftrightarrow",
    "Longleftarrow",
    "Longleftrightarrow",
    "Longrightarrow",
    "Rightarrow",
    "Vdash",
    "Vvdash",
    "approx",
    "asymp",
    "cong",
    "dashv",
    "doteq",
    "downarrow",
    "equiv",
    "ge",
    "geq",
    "geqslant",
    "gets",
    "gg",
    "hookleftarrow",
    "hookrightarrow",
    "iff",
    "implies",
    "in",
    "leadsto",
    "le",
    "leftarrow",
    "leftharpoondown",
    "leftharpoonup",
    "leftrightarrow",
    "leftrightharpoons",
    "leq",
    "leqslant",
    "ll",
    "longleftarrow",
    "longleftrightarrow",
    "longmapsto",
    "longrightarrow",
    "mapsto",
    "mid",
    "models",
    "ne",
    "nearrow",
    "neq",
    "ni",
    "notin",
    "nvdash",
    "nvDash",
    "nwarrow",
    "parallel",
    "perp",
    "prec",
    "preceq",
    "propto",
    "rightharpoondown",
    "rightharpoonup",
    "rightleftharpoons",
    "rightarrow",
    "rightsquigarrow",
    "searrow",
    "sim",
    "simeq",
    "sqsubset",
    "sqsubseteq",
    "sqsupset",
    "sqsupseteq",
    "subset",
    "subseteq",
    "succ",
    "succeq",
    "supset",
    "supseteq",
    "swarrow",
    "to",
    "triangleleft",
    "triangleright",
    "twoheadleftarrow",
    "twoheadrightarrow",
    "uparrow",
    "vdash",
];

const SPACING_COMMANDS: &[&str] = &[
    ",",
    ":",
    ";",
    " ",
    ">",
    "enspace",
    "enskip",
    "medspace",
    "quad",
    "qquad",
    "thickspace",
    "thinspace",
];

const NEGATIVE_SPACING_COMMANDS: &[&str] = &["!", "negmedspace", "negthickspace", "negthinspace"];

const IGNORED_COMMANDS: &[&str] = &[
    "displaystyle",
    "limits",
    "nolimits",
    "scriptstyle",
    "scriptscriptstyle",
    "textstyle",
];

const SIZE_COMMANDS: &[&str] = &[
    "big", "Big", "bigg", "Bigg", "bigl", "Bigl", "biggl", "Biggl", "bigr", "Bigr", "biggr",
    "Biggr",
];

const PLAIN_WRAPPERS: &[&str] = &[
    "emph",
    "mathcal",
    "mathbf",
    "mathfrak",
    "mathit",
    "mathrm",
    "mathnormal",
    "mathscr",
    "mathsf",
    "mathtt",
    "mathup",
    "mbox",
    "overbrace",
    "pmb",
    "smash",
    "substack",
    "text",
    "textbf",
    "textit",
    "textmd",
    "textnormal",
    "textrm",
    "textsc",
    "textsf",
    "textsl",
    "texttt",
    "textup",
    "underbrace",
    "bm",
    "boldsymbol",
];

// Layout markers (upstream PUA markers, latex.ts:512-516 + 627-628).
// They are cleared or resolved before the result is returned.
const LAYOUT_MARKER_START: &str = "\u{f0000}";
const LAYOUT_MARKER_END: &str = "\u{f0001}";
const PROTECTED_SPACE: &str = "\u{f0002}";
const NAMED_OPERATOR_START: &str = "\u{f0004}";
const NAMED_OPERATOR_END: &str = "\u{f0005}";
const NEGATIVE_SPACE: &str = "\u{0}";

/// ECMA-262 `\s` (WhiteSpace + LineTerminator): includes U+FEFF, excludes
/// U+0085 — deliberately not Rust `char::is_whitespace`.
fn is_ecma_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// ECMA-262 `String.prototype.trim` (only trims `\s`, unlike Rust `trim`).
fn trim_ecma_start(s: &str) -> &str {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if !is_ecma_space(c) {
            break;
        }
        end = i + c.len_utf8();
    }
    &s[end..]
}

/// ECMA-262 `String.prototype.trimEnd`.
fn trim_ecma_end(s: &str) -> &str {
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        if !is_ecma_space(c) {
            break;
        }
        start = i;
    }
    &s[..start]
}

/// ECMA-262 `String.prototype.trim`.
fn trim_ecma(s: &str) -> &str {
    trim_ecma_end(trim_ecma_start(s))
}

/// `[\p{L}\p{N}]` (ECMA-262 Unicode property escapes); see the module-level
/// deviation note about `is_alphanumeric`.
fn is_letter_or_number(c: char) -> bool {
    c.is_alphanumeric()
}

fn contains(list: &[&str], value: &str) -> bool {
    list.contains(&value)
}

fn lookup_str<'a>(table: &'a [(&str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Look up a single-character key in a `(char, &str)` table, requiring the
/// value to be exactly one character (upstream object-literal lookup).
fn lookup_single_char<'a>(table: &[(char, &'a str)], value: &str) -> Option<&'a str> {
    let mut chars = value.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    table.iter().find(|(k, _)| *k == first).map(|(_, v)| *v)
}

/// `replaceCharacters` (latex.ts:587-597): map every character, or fail.
fn replace_characters(value: &str, replacements: &[(char, &str)]) -> Option<String> {
    let mut result = String::new();
    for c in value.chars() {
        let replacement = replacements
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, v)| *v)?;
        result.push_str(replacement);
    }
    Some(result)
}

/// `value.replace(/\s*([=+-])\s*/g, "$1")` (latex.ts:602): drop ECMA
/// whitespace immediately around `=`, `+`, `-`.
fn strip_sign_spacing(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '=' || c == '+' || c == '-' {
            while result.chars().next_back().is_some_and(is_ecma_space) {
                result.pop();
            }
            result.push(c);
            while chars.peek().is_some_and(|&next| is_ecma_space(next)) {
                chars.next();
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn is_ascii_alphabetic_all(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphabetic())
}

#[derive(Clone, Copy, PartialEq)]
enum ScriptKind {
    Sub,
    Sup,
}

/// `formatScript` (latex.ts:599-612).
fn format_script(value: &str, kind: ScriptKind) -> String {
    let value = trim_ecma(value);
    let replacements = match kind {
        ScriptKind::Sub => SUBSCRIPTS,
        ScriptKind::Sup => SUPERSCRIPTS,
    };
    let unicode = replace_characters(&strip_sign_spacing(value), replacements);
    if let Some(unicode) = unicode {
        return unicode;
    }

    let prefix = match kind {
        ScriptKind::Sub => "_",
        ScriptKind::Sup => "^",
    };
    if value.chars().count() == 1 || (kind == ScriptKind::Sub && is_ascii_alphabetic_all(value)) {
        format!("{prefix}{value}")
    } else {
        format!("{prefix}({value})")
    }
}

/// `^[\p{L}\p{N}.]+$` (latex.ts:617-619); requires at least one character.
fn is_simple_content(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_alphanumeric() || c == '.')
}

/// `^[\p{N}.]+$` (latex.ts:618); requires at least one character.
fn is_numeric_dot_content(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_numeric() || c == '.')
}

/// `formatFraction` (latex.ts:614-620).
fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = trim_ecma(numerator);
    let denominator = trim_ecma(denominator);
    let simple_numerator = is_simple_content(numerator);
    let simple_denominator =
        is_numeric_dot_content(denominator) || denominator.chars().count() == 1;
    let numerator = if simple_numerator {
        numerator.to_string()
    } else {
        format!("({numerator})")
    };
    let denominator = if simple_denominator {
        denominator.to_string()
    } else {
        format!("({denominator})")
    };
    format!("{numerator}/{denominator}")
}

/// `formatRoot` (latex.ts:622-625).
fn format_root(value: &str, symbol: &str) -> String {
    let value = trim_ecma(value);
    if is_simple_content(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

/// `normalizeOutput` (latex.ts:627-643): named-operator spacing, then
/// whitespace normalization line by line.
fn normalize_output(value: &str) -> String {
    // Pass 1: NAMED_OPERATOR_LEFT_SPACING_PATTERN
    // `(?<=[\p{L}\p{N})\]}\u{f0001}])\u{f0004}` -> " "
    let mut result = String::with_capacity(value.len());
    let mut prev: Option<char> = None;
    for c in value.chars() {
        if c == '\u{f0004}'
            && prev
                .is_some_and(|p| is_letter_or_number(p) || p == ']' || p == '}' || p == '\u{f0001}')
        {
            result.push(' ');
            prev = Some(' ');
            continue;
        }
        result.push(c);
        prev = Some(c);
    }

    // Pass 2: remove all remaining NAMED_OPERATOR_START
    result = result.replace(NAMED_OPERATOR_START, "");

    // Pass 3: NAMED_OPERATOR_RIGHT_SPACING_PATTERN
    // `\u{f0005}(?=[\p{L}\p{N}√\u{f0000}])` -> " "
    let mut spaced = String::with_capacity(result.len());
    let mut chars = result.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{f0005}' {
            match chars.peek() {
                Some(&next) if is_letter_or_number(next) || next == '√' || next == '\u{f0000}' => {
                    spaced.push(' ');
                }
                _ => {}
            }
            continue;
        }
        spaced.push(c);
    }

    // Pass 4: remove all remaining NAMED_OPERATOR_END, then normalize lines.
    let mut lines: Vec<String> = spaced
        .replace(NAMED_OPERATOR_END, "")
        .split('\n')
        .map(|line| collapse_spaces(trim_ecma(line)))
        .collect();
    let count = lines.len();
    let mut result = String::new();
    for (index, line) in lines.drain(..).enumerate() {
        if !line.is_empty() || (index > 0 && index < count - 1) {
            result.push_str(&line);
            result.push('\n');
        }
    }
    trim_ecma(&result).to_string()
}

/// `line.replace(/[ \t]+/g, " ")` (latex.ts:639): collapse space/tab runs.
fn collapse_spaces(line: &str) -> String {
    let mut result = String::new();
    let mut in_run = false;
    for c in line.chars() {
        if c == ' ' || c == '\t' {
            if !in_run {
                result.push(' ');
                in_run = true;
            }
        } else {
            result.push(c);
            in_run = false;
        }
    }
    result
}

enum LayoutNode {
    /// `FractionNode` (latex.ts:645-649).
    Fraction {
        numerator: String,
        denominator: String,
    },
    /// `OperatorNode` (latex.ts:651-656).
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    /// `MatrixNode` (latex.ts:658-662).
    Matrix { lines: Vec<String>, baseline: usize },
}

/// `Layout` (latex.ts:666-670).
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

/// `padLayoutLine` (latex.ts:678-682).
fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{}{}", " ".repeat(left), line, " ".repeat(padding - left))
}

/// `joinLayouts` (latex.ts:684-707).
fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len().saturating_sub(l.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines: Vec<String> = Vec::with_capacity(baseline + below + 1);
    for row in 0..=(baseline + below) {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as isize - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line.push_str(&pad_layout_line(
                    &layout.lines[source_row as usize],
                    layout.width,
                    false,
                ));
            } else {
                line.push_str(&" ".repeat(layout.width));
            }
        }
        lines.push(line.trim_end().to_string());
    }
    Layout {
        lines,
        width: layouts.iter().map(|l| l.width).sum(),
        baseline,
    }
}

/// Find `\u{f0000}(\d+)\u{f0001}` matches (latex.ts:674) as
/// `(start, end, index)` byte ranges.
fn find_layout_markers(line: &str) -> Vec<(usize, usize, usize)> {
    let mut markers = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if line[i..].starts_with(LAYOUT_MARKER_START) {
            let mut j = i + LAYOUT_MARKER_START.len();
            let digits_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start && line[j..].starts_with(LAYOUT_MARKER_END) {
                let end = j + LAYOUT_MARKER_END.len();
                let index: usize = line[digits_start..j].parse().unwrap_or(usize::MAX);
                markers.push((i, end, index));
                i = end;
                continue;
            }
        }
        // Step by whole characters: `i` must stay on char boundaries for the
        // `line[i..]` slicing above.
        let char_len = line[i..].chars().next().map_or(1, |c| c.len_utf8());
        i += char_len;
    }
    markers
}

fn starts_with_ecma_space(s: &str) -> bool {
    s.chars().next().is_some_and(is_ecma_space)
}

fn ends_with_ecma_space(s: &str) -> bool {
    s.chars().next_back().is_some_and(is_ecma_space)
}

/// `renderLayout` (latex.ts:709-795).
fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0;
    for source_line in source.split('\n') {
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0;
        let mut previous_node: Option<&LayoutNode> = None;
        for (start, end, node_index) in find_layout_markers(source_line) {
            let Some(node) = nodes.get(node_index) else {
                // Upstream skips unknown nodes without advancing `position`.
                continue;
            };
            if start > position {
                let sliced = &source_line[position..start];
                let trimmed = if previous_node.is_some() {
                    trim_ecma_start(sliced)
                } else {
                    sliced
                };
                let trimmed = trim_ecma_end(trimmed);
                let preserve_leading_space =
                    matches!(previous_node, Some(LayoutNode::Matrix { .. }))
                        && starts_with_ecma_space(sliced);
                let preserve_trailing_space =
                    matches!(node, LayoutNode::Matrix { .. }) && ends_with_ecma_space(sliced);
                let text = if !trimmed.is_empty() {
                    format!(
                        "{}{}{}",
                        if preserve_leading_space { " " } else { "" },
                        trimmed,
                        if preserve_trailing_space { " " } else { "" }
                    )
                } else if preserve_leading_space || preserve_trailing_space {
                    " ".to_string()
                } else {
                    String::new()
                };
                layouts.push(Layout {
                    lines: vec![text.clone()],
                    width: visible_width(&text),
                    baseline: 0,
                });
            }
            match node {
                LayoutNode::Fraction {
                    numerator,
                    denominator,
                } => {
                    let numerator_layout = render_layout(numerator, nodes);
                    let denominator_layout = render_layout(denominator, nodes);
                    let content_width = numerator_layout.width.max(denominator_layout.width).max(1);
                    let width = content_width + 2;
                    let mut lines: Vec<String> = numerator_layout
                        .lines
                        .iter()
                        .map(|line| pad_layout_line(line, width, true))
                        .collect();
                    lines.push(format!(" {} ", "─".repeat(content_width)));
                    lines.extend(
                        denominator_layout
                            .lines
                            .iter()
                            .map(|line| pad_layout_line(line, width, true)),
                    );
                    layouts.push(Layout {
                        lines,
                        width,
                        baseline: numerator_layout.lines.len(),
                    });
                }
                LayoutNode::Operator {
                    operator,
                    lower,
                    upper,
                } => {
                    let content_width = visible_width(operator)
                        .max(lower.as_deref().map_or(0, visible_width))
                        .max(upper.as_deref().map_or(0, visible_width));
                    let mut lines: Vec<String> = Vec::new();
                    if let Some(upper) = upper {
                        lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
                    }
                    lines.push(format!(
                        "{} ",
                        pad_layout_line(operator, content_width, true)
                    ));
                    if let Some(lower) = lower {
                        lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
                    }
                    layouts.push(Layout {
                        lines,
                        width: content_width + 1,
                        baseline: if upper.is_some() { 1 } else { 0 },
                    });
                }
                LayoutNode::Matrix { lines, baseline } => {
                    let width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
                    layouts.push(Layout {
                        lines: lines
                            .iter()
                            .map(|line| pad_layout_line(line, width, false))
                            .collect(),
                        width,
                        baseline: *baseline,
                    });
                }
            }
            position = end;
            previous_node = Some(node);
        }
        if position < source_line.len() {
            let sliced = &source_line[position..];
            let trimmed = if previous_node.is_some() {
                trim_ecma_start(sliced)
            } else {
                sliced
            };
            let text = if matches!(previous_node, Some(LayoutNode::Matrix { .. }))
                && starts_with_ecma_space(sliced)
            {
                format!(" {trimmed}")
            } else {
                trimmed.to_string()
            };
            layouts.push(Layout {
                lines: vec![text.clone()],
                width: visible_width(&text),
                baseline: 0,
            });
        }
        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    Layout {
        width: rendered_lines
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0),
        lines: rendered_lines,
        baseline: first_baseline,
    }
}

/// Trailing `\u{f0000}(\d+)\u{f0001}` match on the accumulated result
/// (latex.ts:675) — used to append `.` to a trailing matrix line.
fn trailing_layout_marker<'r>(
    result: &str,
    nodes: &'r mut [LayoutNode],
) -> Option<&'r mut LayoutNode> {
    if !result.ends_with(LAYOUT_MARKER_END) {
        return None;
    }
    let tail = &result[..result.len() - LAYOUT_MARKER_END.len()];
    let start_offset = tail.rfind(LAYOUT_MARKER_START)?;
    let digits = &tail[start_offset + LAYOUT_MARKER_START.len()..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: usize = digits.parse().ok()?;
    nodes.get_mut(index)
}

#[derive(Clone, Copy, PartialEq)]
enum LowerStyle {
    Bracket,
    Script,
}

/// `LatexParser` (latex.ts:797-1344).
struct LatexParser<'a> {
    source: &'a str,
    layout_nodes: &'a mut Vec<LayoutNode>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
}

impl<'a> LatexParser<'a> {
    fn new<'b>(
        source: &'b str,
        layout_nodes: &'b mut Vec<LayoutNode>,
        display: bool,
    ) -> LatexParser<'b> {
        LatexParser {
            source,
            layout_nodes,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
        }
    }

    /// `render` (latex.ts:811-817).
    fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn char_at(&self, position: usize) -> Option<char> {
        self.source.get(position..)?.chars().next()
    }

    /// `parseSequence` (latex.ts:819-905).
    fn parse_sequence(&mut self, end_character: Option<char>) -> String {
        let mut result = String::new();
        while self.position < self.source.len() {
            let Some(character) = self.char_at(self.position) else {
                break;
            };
            if let Some(end) = end_character {
                if character == end {
                    self.position += 1;
                    return result;
                }
            }

            if character == '}' {
                self.supported = false;
                return result;
            }

            if character == '{' {
                self.position += 1;
                result.push_str(&self.parse_sequence(Some('}')));
                continue;
            }

            if character == '\\' {
                let command = self.parse_command();
                if command == NEGATIVE_SPACE {
                    result = trim_ecma_end(&result).to_string();
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.truncate(result.len() - NAMED_OPERATOR_END.len());
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }

            if character == '^' || character == '_' {
                self.position += 1;
                result = trim_ecma_end(&result).to_string();
                let script = format_script(
                    &self.parse_required_argument(false),
                    if character == '_' {
                        ScriptKind::Sub
                    } else {
                        ScriptKind::Sup
                    },
                );
                if result.ends_with(NAMED_OPERATOR_END) {
                    let prefix = &result[..result.len() - NAMED_OPERATOR_END.len()];
                    result = format!("{prefix}{script}{NAMED_OPERATOR_END}");
                } else {
                    result.push_str(&script);
                }
                continue;
            }

            if is_ecma_space(character) {
                result.push_str(&self.parse_whitespace());
                continue;
            }

            if character == '=' || character == '<' || character == '>' {
                result = format!("{} {} ", trim_ecma_end(&result), character);
                self.position += 1;
                continue;
            }

            if character == '&' {
                self.position += 1;
                continue;
            }

            if character == '~' {
                self.position += 1;
                result.push(' ');
                continue;
            }

            if character == '.' {
                if let Some(LayoutNode::Matrix { lines, .. }) =
                    trailing_layout_marker(&result, self.layout_nodes)
                {
                    if let Some(last) = lines.last_mut() {
                        last.push('.');
                    }
                    self.position += 1;
                    continue;
                }
            }

            result.push(character);
            self.position += character.len_utf8();
        }

        if end_character.is_some() {
            self.supported = false;
        }
        result
    }

    /// `parseWhitespace` (latex.ts:907-912).
    fn parse_whitespace(&mut self) -> String {
        while let Some(c) = self.char_at(self.position) {
            if !is_ecma_space(c) {
                break;
            }
            self.position += c.len_utf8();
        }
        " ".to_string()
    }

    /// `parseCommand` (latex.ts:914-1079).
    fn parse_command(&mut self) -> String {
        self.position += 1;
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }

        let first = self.char_at(self.position).unwrap_or('\u{fffd}');
        let command: String;
        if first.is_ascii_alphabetic() {
            let start = self.position;
            while let Some(c) = self.char_at(self.position) {
                if !c.is_ascii_alphabetic() {
                    break;
                }
                self.position += 1;
            }
            command = self.source[start..self.position].to_string();
        } else {
            command = first.to_string();
            self.position += first.len_utf8();
        }

        if command == "\\" {
            return "\n".to_string();
        }
        if contains(SPACING_COMMANDS, &command) {
            return " ".to_string();
        }
        if contains(NEGATIVE_SPACING_COMMANDS, &command) {
            return NEGATIVE_SPACE.to_string();
        }
        if contains(IGNORED_COMMANDS, &command) {
            return String::new();
        }
        if matches!(command.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command;
        }
        if command == "|" {
            return "‖".to_string();
        }
        if command == "not" {
            let value = trim_ecma(&self.parse_required_argument(false)).to_string();
            if let Some(negated) = lookup_single_char(NEGATED_SYMBOLS, &value) {
                return format!(" {negated} ");
            }
            let mut characters = value.chars();
            let Some(first_character) = characters.next() else {
                self.supported = false;
                return String::new();
            };
            return format!(" {first_character}\u{0338}{}", characters.as_str());
        }
        if contains(LIMIT_OPERATORS, &command) {
            return self.parse_operator(&command, LowerStyle::Bracket, true, true);
        }
        if let Some(symbol) = lookup_str(SYMBOLS, &command) {
            if contains(DISPLAY_LIMIT_SYMBOLS, &command) {
                return self.parse_operator(symbol, LowerStyle::Script, true, false);
            }
            return if command == "cdot"
                || command == "times"
                || contains(RELATION_COMMANDS, &command)
            {
                format!(" {symbol} ")
            } else {
                symbol.to_string()
            };
        }
        if contains(NAMED_OPERATORS, &command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if contains(SIZE_COMMANDS, &command) {
            return String::new();
        }
        if command == "left" || command == "middle" || command == "right" {
            if self.char_at(self.position) == Some('.') {
                self.position += 1;
            }
            return String::new();
        }
        if command == "frac" || command == "dfrac" || command == "tfrac" {
            let should_stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(!should_stack);
            let denominator = self.parse_required_argument(!should_stack);
            if should_stack {
                let index = self.layout_nodes.len();
                self.layout_nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self.parse_optional_argument();
            let degree = degree.as_deref().map(trim_ecma);
            let value = self.parse_required_argument(true);
            if degree.is_none() || degree == Some("2") {
                return format_root(&value, "√");
            }
            if degree == Some("3") {
                return format_root(&value, "∛");
            }
            if degree == Some("4") {
                return format_root(&value, "∜");
            }
            let Some(degree) = degree else {
                self.supported = false;
                return String::new();
            };
            return format!(
                "{}{}",
                format_script(degree, ScriptKind::Sup),
                format_root(&value, "√")
            );
        }
        if command == "boxed" || command == "fbox" {
            return format!("[{}]", trim_ecma(&self.parse_required_argument(true)));
        }
        if command == "binom" || command == "dbinom" || command == "tbinom" {
            let first = self.parse_required_argument(true);
            let second = self.parse_required_argument(true);
            return format!("({first} choose {second})");
        }
        if let Some(accent) = lookup_str(ACCENTS, &command) {
            let value = self.parse_required_argument(true);
            return if value.chars().count() == 1 {
                format!("{value}{accent}")
            } else {
                format!("{command}({value})")
            };
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            return value
                .chars()
                .map(|c| {
                    BLACKBOARD
                        .iter()
                        .find(|(k, _)| *k == c)
                        .map(|(_, v)| v.to_string())
                        .unwrap_or_else(|| c.to_string())
                })
                .collect();
        }
        if command == "operatorname" {
            let starred = self.char_at(self.position) == Some('*');
            if starred {
                self.position += 1;
            }
            let operator =
                trim_ecma(&normalize_output(&self.parse_required_argument(true))).to_string();
            return self.parse_operator(&operator, LowerStyle::Bracket, starred, true);
        }
        if command == "mod" || command == "bmod" {
            return " mod ".to_string();
        }
        if command == "pmod" || command == "pod" {
            let value = trim_ecma(&self.parse_required_argument(true)).to_string();
            return if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            };
        }
        if command == "overset" || command == "stackrel" {
            let upper = self.parse_required_argument(true);
            let value = trim_ecma(&self.parse_required_argument(true)).to_string();
            return format!("{value}{}", format_script(&upper, ScriptKind::Sup));
        }
        if command == "underset" {
            let lower = self.parse_required_argument(true);
            let value = trim_ecma(&self.parse_required_argument(true)).to_string();
            return format!("{value}{}", format_script(&lower, ScriptKind::Sub));
        }
        if contains(PLAIN_WRAPPERS, &command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                trim_ecma(&value).to_string()
            };
        }
        if command == "begin" {
            return self.parse_environment();
        }
        if command == "end" {
            self.supported = false;
            return String::new();
        }

        self.supported = false;
        format!("\\{command}")
    }

    /// `parseOperator` (latex.ts:1081-1137).
    fn parse_operator(
        &mut self,
        operator: &str,
        inline_lower_style: LowerStyle,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        let mut modifier_position = self.position;
        while self
            .char_at(modifier_position)
            .is_some_and(|c| c == ' ' || c == '\t')
        {
            modifier_position += 1;
        }
        let rest = &self.source[modifier_position..];
        let modifier = if let Some(after) = rest.strip_prefix("\\limits") {
            match after.chars().next() {
                Some(c) if c.is_ascii_alphabetic() => None,
                _ => Some(("\\limits", true)),
            }
        } else if let Some(after) = rest.strip_prefix("\\nolimits") {
            match after.chars().next() {
                Some(c) if c.is_ascii_alphabetic() => None,
                _ => Some(("\\nolimits", false)),
            }
        } else {
            None
        };
        if let Some((text, limits)) = modifier {
            use_display_limits = limits;
            self.position = modifier_position + text.len();
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while self
                .char_at(script_position)
                .is_some_and(|c| c == ' ' || c == '\t')
            {
                script_position += 1;
            }
            let kind = self.char_at(script_position);
            if !matches!(kind, Some('_') | Some('^')) {
                break;
            }
            self.position = script_position + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            if kind == Some('_') {
                if lower.is_some() {
                    self.supported = false;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    self.supported = false;
                }
                upper = Some(value);
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            let index = self.layout_nodes.len();
            self.layout_nodes.push(LayoutNode::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
            return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
        }

        let mut rendered = operator.to_string();
        if let Some(lower) = &lower {
            let lower_rendered = match inline_lower_style {
                LowerStyle::Bracket => format!("[{lower}]"),
                LowerStyle::Script => format_script(lower, ScriptKind::Sub),
            };
            rendered.push_str(&lower_rendered);
        }
        if let Some(upper) = &upper {
            rendered.push_str(&format_script(upper, ScriptKind::Sup));
        }
        if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        }
    }

    /// `parseRequiredArgument` (latex.ts:1139-1145).
    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous_stack_fractions = self.stack_fractions;
        self.stack_fractions = previous_stack_fractions && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous_stack_fractions;
        value
    }

    /// `parseRequiredArgumentValue` (latex.ts:1147-1165).
    fn parse_required_argument_value(&mut self) -> String {
        while self
            .char_at(self.position)
            .is_some_and(|c| c == ' ' || c == '\t')
        {
            self.position += 1;
        }
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }
        let character = self.char_at(self.position).unwrap_or('\u{fffd}');
        if character == '{' {
            self.position += 1;
            return self.parse_sequence(Some('}'));
        }
        if character == '\\' {
            return self.parse_command();
        }
        self.position += character.len_utf8();
        character.to_string()
    }

    /// `parseOptionalArgument` (latex.ts:1167-1182).
    fn parse_optional_argument(&mut self) -> Option<String> {
        while self
            .char_at(self.position)
            .is_some_and(|c| c == ' ' || c == '\t')
        {
            self.position += 1;
        }
        if self.char_at(self.position) != Some('[') {
            return None;
        }
        let end = self.source[self.position + 1..]
            .find(']')
            .map(|i| self.position + 1 + i);
        let Some(end) = end else {
            self.supported = false;
            return None;
        };
        let value = self.source[self.position + 1..end].to_string();
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    /// `readRawGroup` (latex.ts:1184-1212).
    fn read_raw_group(&mut self) -> Option<String> {
        while self
            .char_at(self.position)
            .is_some_and(|c| c == ' ' || c == '\t')
        {
            self.position += 1;
        }
        if self.char_at(self.position) != Some('{') {
            self.supported = false;
            return None;
        }

        self.position += 1;
        let start = self.position;
        let mut depth = 1usize;
        while self.position < self.source.len() {
            let character = self.char_at(self.position).unwrap_or('\u{fffd}');
            if character == '\\' {
                self.position += 1;
                if let Some(next) = self.char_at(self.position) {
                    self.position += next.len_utf8();
                }
                continue;
            }
            if character == '{' {
                depth += 1;
            }
            if character == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value = self.source[start..self.position].to_string();
                self.position += 1;
                return Some(value);
            }
            self.position += character.len_utf8();
        }
        self.supported = false;
        None
    }

    /// `splitEnvironmentRows` (latex.ts:1214-1216): split on
    /// `\\` optionally followed by `[...]` (no `]` or newline inside).
    fn split_environment_rows(body: &str) -> Vec<&str> {
        let mut rows = Vec::new();
        let bytes = body.as_bytes();
        let mut last = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\\') {
                let mut end = i + 2;
                if bytes.get(end) == Some(&b'[') {
                    let mut j = end + 1;
                    while j < bytes.len() && bytes[j] != b']' && bytes[j] != b'\n' {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b']' {
                        end = j + 1;
                    }
                }
                rows.push(&body[last..i]);
                last = end;
                i = end;
                continue;
            }
            i += 1;
        }
        rows.push(&body[last..]);
        rows
    }

    /// `parseEnvironment` (latex.ts:1218-1289).
    fn parse_environment(&mut self) -> String {
        let Some(environment) = self.read_raw_group() else {
            return String::new();
        };
        let end_marker = format!("\\end{{{environment}}}");
        let end = self.source[self.position..]
            .find(&end_marker)
            .map(|offset| self.position + offset);
        let Some(end) = end else {
            self.supported = false;
            return String::new();
        };
        let body = self.source[self.position..end].to_string();
        self.position = end + end_marker.len();

        if environment == "equation" || environment == "equation*" || environment == "displaymath" {
            return trim_ecma(&self.render_nested(&body, true)).to_string();
        }

        if matches!(
            environment.as_str(),
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            let aligned_at = matches!(environment.as_str(), "alignedat" | "alignat" | "alignat*");
            let aligned_body = if aligned_at {
                strip_leading_argument_group(&body)
            } else {
                &body
            };
            return Self::split_environment_rows(aligned_body)
                .into_iter()
                .map(|row| {
                    let cells: Vec<&str> = row.split('&').collect();
                    let source = if aligned_at {
                        (0..cells.len().div_ceil(2))
                            .map(|index| {
                                let start = index * 2;
                                let end = (start + 2).min(cells.len());
                                cells[start..end].concat()
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        cells.concat()
                    };
                    trim_ecma(&self.render_nested(&source, true)).to_string()
                })
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
        }

        if environment == "cases" || environment == "cases*" {
            let rows: Vec<Vec<String>> = Self::split_environment_rows(&body)
                .into_iter()
                .map(|row| {
                    row.split('&')
                        .map(|cell| trim_ecma(&self.render_nested(cell, false)).to_string())
                        .collect()
                })
                .filter(|row: &Vec<String>| row.iter().any(|cell| !cell.is_empty()))
                .collect();
            let mut result = String::new();
            for (index, row) in rows.iter().enumerate() {
                let value = strip_trailing_comma_ws(row.first().map(String::as_str).unwrap_or(""));
                let condition = row.get(1).map(String::as_str).unwrap_or("");
                let delimiter = if index == 0 {
                    "⎧"
                } else if index == rows.len() - 1 {
                    "⎩"
                } else {
                    "⎨"
                };
                let condition_prefix = if starts_with_condition_word(condition) {
                    " "
                } else {
                    " if "
                };
                if condition.is_empty() {
                    result.push_str(&format!("{delimiter} {value}"));
                } else {
                    result.push_str(&format!("{delimiter} {value}{condition_prefix}{condition}"));
                }
                result.push('\n');
            }
            if !result.is_empty() {
                result.pop();
            }
            return result;
        }

        if matches!(
            environment.as_str(),
            "array"
                | "matrix"
                | "smallmatrix"
                | "pmatrix"
                | "bmatrix"
                | "Bmatrix"
                | "vmatrix"
                | "Vmatrix"
        ) {
            let matrix_body = if environment == "array" {
                strip_leading_argument_group(&body)
            } else {
                &body
            };
            return self.render_matrix(&environment, matrix_body);
        }

        self.supported = false;
        body
    }

    /// `renderMatrix` (latex.ts:1291-1334).
    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix: Vec<Vec<String>> = Self::split_environment_rows(body)
            .into_iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| trim_ecma(&self.render_nested(cell, false)).to_string())
                    .collect()
            })
            .filter(|row: &Vec<String>| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        let column_count = matrix.iter().map(|row| row.len()).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix
                    .iter()
                    .map(|row| visible_width(row.get(column).map(String::as_str).unwrap_or("")))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map(String::as_str).unwrap_or("");
                        format!(
                            "{cell}{}",
                            PROTECTED_SPACE
                                .repeat(column_widths[column].saturating_sub(visible_width(cell)))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String>;
        if environment == "array" || environment == "matrix" || environment == "smallmatrix" {
            lines = rows;
        } else {
            let delimiter: Option<[&str; 6]> = match environment {
                "pmatrix" => Some(["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"]),
                "bmatrix" => Some(["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"]),
                "Bmatrix" => Some(["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"]),
                "vmatrix" => Some(["│", "│", "│", "│", "│", "│"]),
                "Vmatrix" => Some(["║", "║", "║", "║", "║", "║"]),
                _ => None,
            };
            let Some(delimiter) = delimiter else {
                self.supported = false;
                return rows.join("\n");
            };
            lines = rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let left = if index == 0 {
                        delimiter[0]
                    } else if index == rows.len() - 1 {
                        delimiter[4]
                    } else {
                        delimiter[2]
                    };
                    let right = if index == 0 {
                        delimiter[1]
                    } else if index == rows.len() - 1 {
                        delimiter[5]
                    } else {
                        delimiter[3]
                    };
                    format!("{left} {row} {right}")
                })
                .collect();
        }

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        let index = self.layout_nodes.len();
        self.layout_nodes
            .push(LayoutNode::Matrix { lines, baseline: 0 });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// `renderNested` (latex.ts:1336-1343).
    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let rendered = LatexParser::new(
            source,
            &mut *self.layout_nodes,
            self.display && stack_fractions,
        )
        .render();
        match rendered {
            Some(rendered) => rendered,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// `body.replace(/^\s*\{[^}]*\}/, "")` (latex.ts:1250, 1283): strip a leading
/// `{...}` argument after leading whitespace; no-op (whitespace kept) when
/// the braces do not close.
fn strip_leading_argument_group(body: &str) -> &str {
    let trimmed = trim_ecma_start(body);
    if !trimmed.starts_with('{') {
        return body;
    }
    let Some(end) = trimmed.find('}') else {
        return body;
    };
    let leading = body.len() - trimmed.len();
    &body[leading + end + 1..]
}

/// `,\s*$` (latex.ts:1271): strip a trailing comma and the whitespace after it.
fn strip_trailing_comma_ws(value: &str) -> &str {
    trim_ecma_end(value)
        .strip_suffix(',')
        .unwrap_or_else(|| trim_ecma_end(value))
}

/// `/^(?:if|when|for|otherwise)\b/i` (latex.ts:1274).
fn starts_with_condition_word(condition: &str) -> bool {
    for word in ["if", "when", "for", "otherwise"] {
        if condition
            .get(..word.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
        {
            match condition[word.len()..].chars().next() {
                None => return true,
                Some(next) => {
                    if !next.is_ascii_alphanumeric() && next != '_' {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Render a basic LaTeX math expression as terminal-friendly Unicode text
/// (upstream `renderLatex`, latex.ts:1355-1373). Returns `None` when the
/// expression contains unsupported or malformed syntax.
///
/// With `display` set, fractions and operator limits stack vertically.
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    let mut layout_nodes: Vec<LayoutNode> = Vec::new();
    let rendered = LatexParser::new(source, &mut layout_nodes, display).render()?;
    if layout_nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    let lines = render_layout(&rendered, &layout_nodes).lines;
    let indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    Some(
        lines
            .iter()
            .map(|line| line.get(indentation..).unwrap_or("").trim_end())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .replace(PROTECTED_SPACE, " "),
    )
}

#[cfg(test)]
mod tests {
    use super::render_latex;

    fn render(source: &str) -> Option<String> {
        render_latex(source, false)
    }

    /// Upstream `defineCases` helper: one assertion per (source, expected) pair.
    fn define_cases(cases: &[(&str, &str)]) {
        for (source, expected) in cases {
            assert_eq!(
                render(source).as_deref(),
                Some(*expected),
                "source: {source:?}"
            );
        }
    }

    /// Port of the upstream defineCases table in `describe("Jacobian conjecture session using dollar delimiters")`.
    #[test]
    fn jacobian_conjecture_session_using_dollar_delimiters() {
        define_cases(&[
            ("\\mathbb{C}^3 \\to \\mathbb{C}^3", "ℂ³ → ℂ³"),
            ("\\{3x+2y,\\; 27x^2-4z-1,\\; x(x-1)(x+1)\\} \\quad\\Rightarrow\\quad x \\in \\{0, \\pm 1\\},", "{3x+2y, 27x²-4z-1, x(x-1)(x+1)} ⇒ x ∈ {0, ± 1},"),
            ("F_1 = -\\frac{1}{4x^2}.", "F₁ = -1/(4x²)."),
            ("-2", "-2"),
            ("(0,0,-1/4)", "(0,0,-1/4)"),
            ("(1,-3/2,13/2)", "(1,-3/2,13/2)"),
            ("(1,1,1)", "(1,1,1)"),
            ("(2,1,0)", "(2,1,0)"),
            ("(-1/4, 0, 0)", "(-1/4, 0, 0)"),
            ("\\{(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2)\\}", "{(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2)}"),
            ("(2,1,1)", "(2,1,1)"),
            ("(7/3,-2/5,11/7)", "(7/3,-2/5,11/7)"),
            ("\\{y - p(x),\\; q(x)\\}", "{y - p(x), q(x)}"),
            ("\\deg q = 3", "deg q = 3"),
            ("[\\mathbb{C}(x,y,z):\\mathbb{C}(F_1,F_2,F_3)] = 3", "[ℂ(x,y,z):ℂ(F₁,F₂,F₃)] = 3"),
            ("u = 1+xy", "u = 1+xy"),
            ("G = u^2 z + y^2(4+3xy)", "G = u² z + y²(4+3xy)"),
            ("F_1 = uG", "F₁ = uG"),
            ("F_2 = y + 3xG", "F₂ = y + 3xG"),
            ("x=0", "x = 0"),
            ("F_2 = F_3 = 0", "F₂ = F₃ = 0"),
            ("xy = -3/2", "xy = -3/2"),
            ("x^2 z = 13/2", "x² z = 13/2"),
            ("\\mathbb{C}^*", "ℂ^*"),
            ("s \\mapsto (s,\\, -\\tfrac{3}{2s},\\, \\tfrac{13}{2s^2})", "s ↦ (s, -3/(2s), 13/(2s²))"),
            ("X", "X"),
            ("p_\\pm", "p_±"),
            ("F(-x,-y,z) = (F_1, -F_2, -F_3)", "F(-x,-y,z) = (F₁, -F₂, -F₃)"),
            ("p_0", "p₀"),
            ("s \\to \\infty", "s → ∞"),
            ("(0,0,0)", "(0,0,0)"),
            ("\\Rightarrow", "⇒"),
            ("\\ge 2", "≥ 2"),
            ("\\ge 3", "≥ 3"),
            ("1", "1"),
            ("\\mathrm{diag}(-1/2,1,1)", "diag(-1/2,1,1)"),
            ("4+3xy", "4+3xy"),
        ]);
    }

    /// Port of the upstream defineCases table in `describe("satellite calculation session using bracket delimiters")`.
    #[test]
    fn satellite_calculation_session_using_bracket_delimiters() {
        define_cases(&[
            ("E \\approx \\frac{0.1\\ \\text{lux}}{100\\ \\text{lm/W}} = 0.001\\ \\text{W/m}^2", "E ≈ (0.1 lux)/(100 lm/W) = 0.001 W/m²"),
            ("\\boxed{1\\ \\text{milliwatt per square metre}}", "[1 milliwatt per square metre]"),
            ("5\\ \\text{km}^2 = 5{,}000{,}000\\ \\text{m}^2", "5 km² = 5,000,000 m²"),
            ("P_{\\text{light}} = 0.001 \\times 5{,}000{,}000\n= \\boxed{5{,}000\\ \\text{W}}", "P_light = 0.001 × 5,000,000 = [5,000 W]"),
            ("P_{\\text{electric}} = 5\\ \\text{kW} \\times 0.2\n= \\boxed{1\\ \\text{kW}}", "P_electric = 5 kW × 0.2 = [1 kW]"),
            ("\\pi(2.5\\ \\text{km})^2 = 19.6\\ \\text{km}^2", "π(2.5 km)² = 19.6 km²"),
            ("0.001\\ \\text{W/m}^2 \\times 19.6 \\times 10^6\\ \\text{m}^2\n\\approx \\boxed{20\\ \\text{kW optical}}", "0.001 W/m² × 19.6 × 10⁶ m² ≈ [20 kW optical]"),
            ("1\\ \\text{kW} \\times \\frac{1}{3600}\\ \\text{hour}\n= \\boxed{0.28\\ \\text{Wh}}", "1 kW × 1/3600 hour = [0.28 Wh]"),
        ]);
    }

    /// Port of the upstream defineCases table in `describe("Jacobian conjecture sessions using parenthesis and bracket delimiters")`.
    #[test]
    fn jacobian_conjecture_sessions_using_parenthesis_and_bracket_delimiters() {
        define_cases(&[
            ("\\det\\!\\left(\\frac{\\partial(F_1,F_2,F_3)}{\\partial(x,y,z)}\\right)=-2.", "det((∂(F₁,F₂,F₃))/(∂(x,y,z))) = -2."),
            ("\\begin{aligned}\nF(0,0,-\\tfrac14)&=(-\\tfrac14,0,0),\\\\\nF(1,-\\tfrac32,\\tfrac{13}2)&=(-\\tfrac14,0,0),\\\\\nF(-1,\\tfrac32,\\tfrac{13}2)&=(-\\tfrac14,0,0).\n\\end{aligned}", "F(0,0,-1/4) = (-1/4,0,0),\nF(1,-3/2,13/2) = (-1/4,0,0),\nF(-1,3/2,13/2) = (-1/4,0,0)."),
            ("F=(F_1,F_2,F_3)", "F = (F₁,F₂,F₃)"),
            ("F", "F"),
            ("3", "3"),
        ]);
    }

    /// Port of the upstream defineCases table in `describe("Jacobian matrix session using dollar delimiters")`.
    #[test]
    fn jacobian_matrix_session_using_dollar_delimiters() {
        define_cases(&[
            ("J = \\begin{pmatrix}\n\\frac{\\partial f_1}{\\partial x} & \\frac{\\partial f_1}{\\partial y} & \\frac{\\partial f_1}{\\partial z} \\\\\n\\frac{\\partial f_2}{\\partial x} & \\frac{\\partial f_2}{\\partial y} & \\frac{\\partial f_2}{\\partial z} \\\\\n\\frac{\\partial f_3}{\\partial x} & \\frac{\\partial f_3}{\\partial y} & \\frac{\\partial f_3}{\\partial z}\n\\end{pmatrix}", "J = ⎛ (∂ f₁)/(∂ x) │ (∂ f₁)/(∂ y) │ (∂ f₁)/(∂ z) ⎞\n    ⎜ (∂ f₂)/(∂ x) │ (∂ f₂)/(∂ y) │ (∂ f₂)/(∂ z) ⎟\n    ⎝ (∂ f₃)/(∂ x) │ (∂ f₃)/(∂ y) │ (∂ f₃)/(∂ z) ⎠"),
            ("\\begin{aligned}\nf_1 &= (1+xy)^3 z + y^2(1+xy)(4+3xy) \\\\\nf_2 &= y + 3x(1+xy)^2 z + 3xy^2(4+3xy) \\\\\nf_3 &= 2x - 3x^2y - x^3z\n\\end{aligned}", "f₁ = (1+xy)³ z + y²(1+xy)(4+3xy)\nf₂ = y + 3x(1+xy)² z + 3xy²(4+3xy)\nf₃ = 2x - 3x²y - x³z"),
            ("x, y, z", "x, y, z"),
            ("(x, y, z)", "(x, y, z)"),
            ("(0,\\; 0,\\; -\\tfrac14)", "(0, 0, -1/4)"),
            ("(-\\tfrac14,\\; 0,\\; 0)", "(-1/4, 0, 0)"),
            ("(1,\\; -\\tfrac32,\\; \\tfrac{13}{2})", "(1, -3/2, 13/2)"),
            ("(-1,\\; \\tfrac32,\\; \\tfrac{13}{2})", "(-1, 3/2, 13/2)"),
            ("(-\\frac14, 0, 0)", "(-1/4, 0, 0)"),
            ("F: \\mathbb{C}^3 \\to \\mathbb{C}^3", "F: ℂ³ → ℂ³"),
            ("F(0,0,-\\tfrac14) = F(1,-\\tfrac32,\\tfrac{13}{2}) = F(-1,\\tfrac32,\\tfrac{13}{2}) = (-\\tfrac14, 0, 0)", "F(0,0,-1/4) = F(1,-3/2,13/2) = F(-1,3/2,13/2) = (-1/4, 0, 0)"),
            ("\\mathbb{C}^3", "ℂ³"),
            ("\\begin{aligned}\nf_1 &= \\frac{f_1^{\\text{ut}}(u,t)}{x^2}, \\quad\nf_2 = \\frac{f_2^{\\text{ut}}(u,t)}{x}, \\quad\nf_3 = x\\,(2 - 3u - t)\n\\end{aligned}", "f₁ = (f₁ᵘᵗ(u,t))/(x²), f₂ = (f₂ᵘᵗ(u,t))/x, f₃ = x (2 - 3u - t)"),
            ("\\det J_F", "det J_F"),
            ("(-\\tfrac14, 0, 0)", "(-1/4, 0, 0)"),
            ("u = xy", "u = xy"),
            ("t = x^2z", "t = x²z"),
            ("x \\neq 0", "x ≠ 0"),
            ("f_1^{\\text{ut}}, f_2^{\\text{ut}}", "f₁ᵘᵗ, f₂ᵘᵗ"),
            ("u,t", "u,t"),
            ("x", "x"),
            ("x, x^2", "x, x²"),
            ("\\mathbb{C}^n \\to \\mathbb{C}^n", "ℂⁿ → ℂⁿ"),
            ("n \\geq 2", "n ≥ 2"),
            ("\\mathbb{P}^3", "ℙ³"),
        ]);
    }

    /// Port of the upstream defineCases table in `describe("extended formulas from a renderer stress-test session")`.
    #[test]
    fn extended_formulas_from_a_renderer_stress_test_session() {
        define_cases(&[
            ("e^{i\\pi}+1=0", "e^(iπ)+1 = 0"),
            ("\\boxed{\n\\mathcal{Z}(\\beta)\n=\n\\int_{\\mathcal M}\n\\exp\\!\\left(\n-\\beta\\left[\n\\frac12 g^{ij}(x)\\,\\partial_i\\phi\\,\\partial_j\\phi\n+V(\\phi)\n\\right]\\right)\n\\mathcal D\\phi\n}", "[Z(β) = ∫_M exp( -β[ 1/2 gⁱʲ(x) ∂ᵢϕ ∂ⱼϕ +V(ϕ) ]) Dϕ]"),
            ("\\begin{aligned}\n\\nabla_\\mu T^{\\mu\\nu}\n&=\n\\frac{1}{\\sqrt{-g}}\n\\partial_\\mu\\!\\left(\\sqrt{-g}\\,T^{\\mu\\nu}\\right)\n+\\Gamma^\\nu_{\\mu\\lambda}T^{\\mu\\lambda}\n=0, \\\\[4pt]\nR_{\\mu\\nu}-\\frac12 Rg_{\\mu\\nu}+\\Lambda g_{\\mu\\nu}\n&=\n\\frac{8\\pi G}{c^4}T_{\\mu\\nu}.\n\\end{aligned}", "∇_μ T^(μν) = 1/(√(-g)) ∂_μ(√(-g) T^(μν)) +Γ^ν_(μλ)T^(μλ) = 0,\nR_(μν)-1/2 Rg_(μν)+Λ g_(μν) = (8π G)/(c⁴)T_(μν)."),
            ("f(z)\n=\n\\frac{1}{2\\pi i}\n\\oint_{\\gamma}\n\\frac{f(\\zeta)}{\\zeta-z}\\,d\\zeta,\n\\qquad\n\\det\\!\\begin{pmatrix}\n\\lambda-a & -b & 0\\\\\n-c & \\lambda-d & -e\\\\\n0 & -f & \\lambda-g\n\\end{pmatrix}\n=0.", "f(z) = 1/(2π i) ∮_γ (f(ζ))/(ζ-z) dζ, det⎛ λ-a │ -b  │ 0   ⎞ = 0.\n                                        ⎜ -c  │ λ-d │ -e  ⎟\n                                        ⎝ 0   │ -f  │ λ-g ⎠"),
            ("\\Psi(x,t)=\n\\sum_{n=1}^{\\infty}\n\\underbrace{\nc_n\n\\sqrt{\\frac{2}{L}}\n\\sin\\!\\left(\\frac{n\\pi x}{L}\\right)\n}_{\\text{spatial eigenmode}}\n\\exp\\!\\left(-\\frac{i\\hbar n^2\\pi^2}{2mL^2}t\\right),\n\\qquad\n|\\Psi(x,t)|^2\n=\n\\begin{cases}\n\\Psi^\\ast\\Psi, & 0<x<L,\\\\\n0, & \\text{otherwise}.\n\\end{cases}", "Ψ(x,t) = ∑ₙ₌₁^∞ cₙ √(2/L) sin((nπ x)/L)_(spatial eigenmode) exp(-(iℏ n²π²)/(2mL²)t), |Ψ(x,t)|² = ⎧ Ψ^∗Ψ if 0 < x < L,\n⎩ 0 otherwise."),
            ("x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}", "x = (-b±√(b²-4ac))/(2a)"),
            ("\\int_0^\\infty e^{-x^2}\\,dx=\\frac{\\sqrt{\\pi}}{2}", "∫₀^∞ e^(-x²) dx = (√π)/2"),
            ("e^{i\\theta}=\\cos\\theta+i\\sin\\theta", "e^(iθ) = cos θ+i sin θ"),
            ("\\sum_{n=1}^{\\infty}\\frac{1}{n^2}=\\frac{\\pi^2}{6}", "∑ₙ₌₁^∞1/(n²) = π²/6"),
            ("\\lim_{x\\to 0}\\frac{\\sin x}{x}=1", "lim[x→0] (sin x)/x = 1"),
            ("\\lim_{n\\to\\infty}\n\\left(1+\\frac{1}{n}\\right)^n=e", "lim[n→∞] (1+1/n)ⁿ = e"),
            ("\\int_0^1 \\frac{x^2}{1+x^3}\\,dx\n=\\frac{1}{3}\\ln 2", "∫₀¹ x²/(1+x³) dx = 1/3 ln 2"),
            ("\\sum_{k=1}^{n}\\frac{k}{k+1}\n=n+1-H_{n+1}", "∑ₖ₌₁ⁿk/(k+1) = n+1-Hₙ₊₁"),
            ("\\frac{\n  \\displaystyle \\frac{x^2+1}{x-1}\n  -\n  \\displaystyle \\frac{2x}{x+1}\n}{\n  \\displaystyle \\frac{x}{x^2-1}\n}", "((x²+1)/(x-1) - 2x/(x+1))/(x/(x²-1))"),
            ("\\lim_{x\\to 0}\n\\frac{\n  \\displaystyle \\frac{\\sin x}{x}-1\n}{\n  \\displaystyle \\frac{e^x-1}{x}-1\n}\n=0", "lim[x→0] ((sin x)/x-1)/((eˣ-1)/x-1) = 0"),
            ("\\frac{\n  1+\\displaystyle\\frac{1}{1+\\frac{1}{x}}\n}{\n  1-\\displaystyle\\frac{1}{1-\\frac{1}{x}}\n}", "(1+1/(1+1/x))/(1-1/(1-1/x))"),
            ("\\sum_{n=1}^{\\infty}\n\\frac{\n  \\displaystyle \\frac{1}{n}-\\frac{1}{n+1}\n}{\n  \\displaystyle 1+\\frac{1}{n^2}\n}", "∑ₙ₌₁^∞ (1/n-1/(n+1))/(1+1/(n²))"),
        ]);
    }

    /// Port of the upstream `it("renders common symbols, roots, sums, and integrals")`.
    #[test]
    fn renders_common_symbols_roots_sums_and_integrals() {
        assert_eq!(
            render("\\sum_{i=0}^n \\alpha_i + \\int_0^\\infty e^{-x^2}\\,dx = \\sqrt{\\pi}")
                .as_deref(),
            Some("∑ᵢ₌₀ⁿ αᵢ + ∫₀^∞ e^(-x²) dx = √π"),
            "source: {:?}",
            "\\sum_{i=0}^n \\alpha_i + \\int_0^\\infty e^{-x^2}\\,dx = \\sqrt{\\pi}"
        );
    }

    /// Port of the upstream `it("renders common accents and binomial notation")`.
    #[test]
    fn renders_common_accents_and_binomial_notation() {
        assert_eq!(
            render("\\binom{n}{k}+\\vec{x}+\\hat{y}+\\overline{AB}").as_deref(),
            Some("(n choose k)+x⃗+ŷ+overline(AB)"),
            "source: {:?}",
            "\\binom{n}{k}+\\vec{x}+\\hat{y}+\\overline{AB}"
        );
    }

    /// Port of the upstream `it("renders extended symbols and negated relations")`.
    #[test]
    fn renders_extended_symbols_and_negated_relations() {
        assert_eq!(render("\\epsilon+\\varepsilon+\\varsigma+\\varkappa+\\oplus+\\otimes+\\therefore+\\because").as_deref(), Some("ϵ+ε+ς+ϰ+⊕+⊗+∴+∵"), "source: {:?}", "\\epsilon+\\varepsilon+\\varsigma+\\varkappa+\\oplus+\\otimes+\\therefore+\\because");
        assert_eq!(
            render("A\\not\\subseteq B,\\quad x\\not\\in X").as_deref(),
            Some("A ⊈ B, x ∉ X"),
            "source: {:?}",
            "A\\not\\subseteq B,\\quad x\\not\\in X"
        );
    }

    /// Port of the upstream `it("renders delimiter commands and invisible delimiters")`.
    #[test]
    fn renders_delimiter_commands_and_invisible_delimiters() {
        assert_eq!(
            render("\\lvert{x}\\rvert+\\lVert{v}\\rVert+\\left.\\frac{dy}{dx}\\right|_{x=0}")
                .as_deref(),
            Some("|x|+‖v‖+dy/(dx)|ₓ₌₀"),
            "source: {:?}",
            "\\lvert{x}\\rvert+\\lVert{v}\\rVert+\\left.\\frac{dy}{dx}\\right|_{x=0}"
        );
        assert_eq!(
            render("\\left\\lbrace x \\middle| x>0 \\right\\rbrace").as_deref(),
            Some("{ x | x > 0 }"),
            "source: {:?}",
            "\\left\\lbrace x \\middle| x>0 \\right\\rbrace"
        );
    }

    /// Port of the upstream `it("renders named, modular, overlaid, and underlaid operators")`.
    #[test]
    fn renders_named_modular_overlaid_and_underlaid_operators() {
        assert_eq!(
            render("\\operatorname*{arg\\,max}_{x\\in X} f(x)").as_deref(),
            Some("arg max[x∈X] f(x)"),
            "source: {:?}",
            "\\operatorname*{arg\\,max}_{x\\in X} f(x)"
        );
        assert_eq!(
            render("a\\bmod n,\\quad a\\equiv b\\pmod n").as_deref(),
            Some("a mod n, a ≡ b (mod n)"),
            "source: {:?}",
            "a\\bmod n,\\quad a\\equiv b\\pmod n"
        );
        assert_eq!(
            render("\\overset{!}{=}+\\underset{n}{x}+\\stackrel{def}{=}").as_deref(),
            Some("=^!+xₙ+=ᵈᵉᶠ"),
            "source: {:?}",
            "\\overset{!}{=}+\\underset{n}{x}+\\stackrel{def}{=}"
        );
    }

    /// Port of the upstream `it("renders indexed roots and additional accents and wrappers")`.
    #[test]
    fn renders_indexed_roots_and_additional_accents_and_wrappers() {
        assert_eq!(
            render("\\sqrt[2]{x}+\\sqrt[3]{x}+\\sqrt[4]{x}+\\sqrt[n]{x}+\\sqrt[k]{x+1}").as_deref(),
            Some("√x+∛x+∜x+ⁿ√x+ᵏ√(x+1)"),
            "source: {:?}",
            "\\sqrt[2]{x}+\\sqrt[3]{x}+\\sqrt[4]{x}+\\sqrt[n]{x}+\\sqrt[k]{x+1}"
        );
        assert_eq!(
            render("\\acute{x}+\\grave{y}+\\widehat{xyz}+\\overrightarrow{AB}").as_deref(),
            Some("x́+ỳ+widehat(xyz)+overrightarrow(AB)"),
            "source: {:?}",
            "\\acute{x}+\\grave{y}+\\widehat{xyz}+\\overrightarrow{AB}"
        );
        assert_eq!(
            render("\\textnormal{hello}+\\mbox{world}+\\boldsymbol{x}").as_deref(),
            Some("hello+world+x"),
            "source: {:?}",
            "\\textnormal{hello}+\\mbox{world}+\\boldsymbol{x}"
        );
    }

    /// Port of the upstream `it("renders additional display environments")`.
    #[test]
    fn renders_additional_display_environments() {
        assert_eq!(
            render("\\begin{equation}\\begin{split}a&=b\\\\&=c\\end{split}\\end{equation}")
                .as_deref(),
            Some("a = b\n= c"),
            "source: {:?}",
            "\\begin{equation}\\begin{split}a&=b\\\\&=c\\end{split}\\end{equation}"
        );
        assert_eq!(
            render("\\begin{alignedat}{2}a&=b&\\quad c&=d\\\\e&=f&g&=h\\end{alignedat}").as_deref(),
            Some("a = b c = d\ne = f g = h"),
            "source: {:?}",
            "\\begin{alignedat}{2}a&=b&\\quad c&=d\\\\e&=f&g&=h\\end{alignedat}"
        );
    }

    /// Port of the upstream `it("uses natural case conditions and aligns matrix columns")`.
    #[test]
    fn uses_natural_case_conditions_and_aligns_matrix_columns() {
        assert_eq!(render("\\begin{cases}a & x<0 \\\\ b & \\text{if }x=0 \\\\ c & \\text{otherwise}\\end{cases}").as_deref(), Some("⎧ a if x < 0\n⎨ b if x = 0\n⎩ c otherwise"), "source: {:?}", "\\begin{cases}a & x<0 \\\\ b & \\text{if }x=0 \\\\ c & \\text{otherwise}\\end{cases}");
        assert_eq!(
            render("\\begin{pmatrix}1&200\\\\3000&4\\end{pmatrix}").as_deref(),
            Some("⎛ 1    │ 200 ⎞\n⎝ 3000 │ 4   ⎠"),
            "source: {:?}",
            "\\begin{pmatrix}1&200\\\\3000&4\\end{pmatrix}"
        );
    }

    /// Port of the upstream `it("composes matrices with fractions and adjacent matrices")`.
    #[test]
    fn composes_matrices_with_fractions_and_adjacent_matrices() {
        assert_eq!(render_latex("R\\left(\\frac{\\pi}{4}\\right)\n=\n\\begin{pmatrix}\n\\frac{\\sqrt{2}}{2} & -\\frac{\\sqrt{2}}{2}\\\\\n\\frac{\\sqrt{2}}{2} & \\frac{\\sqrt{2}}{2}\n\\end{pmatrix}.", true).as_deref(), Some("   π\nR( ─ ) = ⎛ (√2)/2 │ -(√2)/2 ⎞\n   4     ⎝ (√2)/2 │ (√2)/2  ⎠."), "source: {:?}", "R\\left(\\frac{\\pi}{4}\\right)\n=\n\\begin{pmatrix}\n\\frac{\\sqrt{2}}{2} & -\\frac{\\sqrt{2}}{2}\\\\\n\\frac{\\sqrt{2}}{2} & \\frac{\\sqrt{2}}{2}\n\\end{pmatrix}.");
        assert_eq!(render_latex("\\mathbf w\n=\nR\\left(\\frac{\\pi}{4}\\right)\n\\begin{pmatrix}1\\\\0\\end{pmatrix}\n=\n\\begin{pmatrix}\\frac{\\sqrt{2}}{2}\\\\\\frac{\\sqrt{2}}{2}\\end{pmatrix}.", true).as_deref(), Some("       π\nw = R( ─ ) ⎛ 1 ⎞ = ⎛ (√2)/2 ⎞\n       4   ⎝ 0 ⎠   ⎝ (√2)/2 ⎠."), "source: {:?}", "\\mathbf w\n=\nR\\left(\\frac{\\pi}{4}\\right)\n\\begin{pmatrix}1\\\\0\\end{pmatrix}\n=\n\\begin{pmatrix}\\frac{\\sqrt{2}}{2}\\\\\\frac{\\sqrt{2}}{2}\\end{pmatrix}.");
        assert_eq!(render_latex("A\\mathbf e_1=\\begin{pmatrix}\\pi\\\\0\\end{pmatrix},\\qquad A\\mathbf e_2=\\begin{pmatrix}0\\\\\\frac{1}{\\pi}\\end{pmatrix}.", true).as_deref(), Some("Ae₁ = ⎛ π ⎞, Ae₂ = ⎛ 0   ⎞\n      ⎝ 0 ⎠        ⎝ 1/π ⎠."), "source: {:?}", "A\\mathbf e_1=\\begin{pmatrix}\\pi\\\\0\\end{pmatrix},\\qquad A\\mathbf e_2=\\begin{pmatrix}0\\\\\\frac{1}{\\pi}\\end{pmatrix}.");
        assert_eq!(
            render_latex(
                "\\sum_{i=0}^n x_i=\\begin{pmatrix}a&b\\\\c&d\\end{pmatrix}.",
                true
            )
            .as_deref(),
            Some(" n\n ∑  xᵢ = ⎛ a │ b ⎞\ni=0      ⎝ c │ d ⎠."),
            "source: {:?}",
            "\\sum_{i=0}^n x_i=\\begin{pmatrix}a&b\\\\c&d\\end{pmatrix}."
        );
    }

    /// Port of the upstream `it("normalizes relation, multiplication, and named-operator spacing")`.
    #[test]
    fn normalizes_relation_multiplication_and_named_operator_spacing() {
        assert_eq!(
            render("x=y").as_deref(),
            Some("x = y"),
            "source: {:?}",
            "x=y"
        );
        assert_eq!(
            render("x =y").as_deref(),
            Some("x = y"),
            "source: {:?}",
            "x =y"
        );
        assert_eq!(
            render("x=\ny").as_deref(),
            Some("x = y"),
            "source: {:?}",
            "x=\ny"
        );
        assert_eq!(
            render("x\n=\ny").as_deref(),
            Some("x = y"),
            "source: {:?}",
            "x\n=\ny"
        );
        assert_eq!(
            render("x_{i=0}").as_deref(),
            Some("xᵢ₌₀"),
            "source: {:?}",
            "x_{i=0}"
        );
        assert_eq!(
            render("x\\neq0").as_deref(),
            Some("x ≠ 0"),
            "source: {:?}",
            "x\\neq0"
        );
        assert_eq!(
            render("A\\to B").as_deref(),
            Some("A → B"),
            "source: {:?}",
            "A\\to B"
        );
        assert_eq!(
            render("\\pi\\cdot\\frac{1}{\\pi}").as_deref(),
            Some("π · 1/π"),
            "source: {:?}",
            "\\pi\\cdot\\frac{1}{\\pi}"
        );
        assert_eq!(
            render("\\sin\\theta").as_deref(),
            Some("sin θ"),
            "source: {:?}",
            "\\sin\\theta"
        );
        assert_eq!(
            render("\\sin^2 x").as_deref(),
            Some("sin² x"),
            "source: {:?}",
            "\\sin^2 x"
        );
        assert_eq!(
            render("-\\sin\\theta").as_deref(),
            Some("-sin θ"),
            "source: {:?}",
            "-\\sin\\theta"
        );
        assert_eq!(
            render("i\\sin\\theta").as_deref(),
            Some("i sin θ"),
            "source: {:?}",
            "i\\sin\\theta"
        );
        assert_eq!(
            render("\\det(A)").as_deref(),
            Some("det(A)"),
            "source: {:?}",
            "\\det(A)"
        );
    }

    /// Port of the upstream `it("stacks operator limits in display mode")`.
    #[test]
    fn stacks_operator_limits_in_display_mode() {
        assert_eq!(
            render_latex("\\sum_{i=0}^n x_i", true).as_deref(),
            Some(" n\n ∑  xᵢ\ni=0"),
            "source: {:?}",
            "\\sum_{i=0}^n x_i"
        );
        assert_eq!(
            render_latex("\\min_{x\\in X} f(x)", true).as_deref(),
            Some("min f(x)\nx∈X"),
            "source: {:?}",
            "\\min_{x\\in X} f(x)"
        );
        assert_eq!(
            render_latex("\\operatorname*{arg\\,max}_{x\\in X} f(x)", true).as_deref(),
            Some("arg max f(x)\n  x∈X"),
            "source: {:?}",
            "\\operatorname*{arg\\,max}_{x\\in X} f(x)"
        );
        assert_eq!(
            render_latex("\\int\\nolimits_0^1 f(x)\\,dx", true).as_deref(),
            Some("∫₀¹ f(x) dx"),
            "source: {:?}",
            "\\int\\nolimits_0^1 f(x)\\,dx"
        );
        assert_eq!(
            render_latex("\\int\\limits_0^1 f(x)\\,dx", true).as_deref(),
            Some("1\n∫ f(x) dx\n0"),
            "source: {:?}",
            "\\int\\limits_0^1 f(x)\\,dx"
        );
    }

    /// Port of the upstream `it("uses the middle brace for intermediate case rows")`.
    #[test]
    fn uses_the_middle_brace_for_intermediate_case_rows() {
        assert_eq!(
            render("\\begin{cases}a & x<0 \\\\ b & x=0 \\\\ c & x>0\\end{cases}").as_deref(),
            Some("⎧ a if x < 0\n⎨ b if x = 0\n⎩ c if x > 0"),
            "source: {:?}",
            "\\begin{cases}a & x<0 \\\\ b & x=0 \\\\ c & x>0\\end{cases}"
        );
    }

    /// Port of the upstream `it("stacks fractions in display mode")`.
    #[test]
    fn stacks_fractions_in_display_mode() {
        assert_eq!(
            render_latex("x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}", true).as_deref(),
            Some("    -b±√(b²-4ac)\nx = ────────────\n         2a"),
            "source: {:?}",
            "x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}"
        );
        assert_eq!(
            render_latex("\\frac{x^2+1}{x-1}", true).as_deref(),
            Some("x²+1\n────\nx-1"),
            "source: {:?}",
            "\\frac{x^2+1}{x-1}"
        );
    }

    /// Port of the upstream `it("keeps nested display fractions linear")`.
    #[test]
    fn keeps_nested_display_fractions_linear() {
        assert_eq!(
            render_latex(
                "\\frac{\\frac{x^2+1}{x-1}-\\frac{2x}{x+1}}{\\frac{x}{x^2-1}}",
                true
            )
            .as_deref(),
            Some("(x²+1)/(x-1)-2x/(x+1)\n─────────────────────\n      x/(x²-1)"),
            "source: {:?}",
            "\\frac{\\frac{x^2+1}{x-1}-\\frac{2x}{x+1}}{\\frac{x}{x^2-1}}"
        );
        assert_eq!(
            render_latex(
                "\\lim_{x\\to 0}\\frac{\\frac{\\sin x}{x}-1}{\\frac{e^x-1}{x}-1}=0",
                true
            )
            .as_deref(),
            Some("     (sin x)/x-1\nlim  ─────────── = 0\nx→0  (eˣ-1)/x-1"),
            "source: {:?}",
            "\\lim_{x\\to 0}\\frac{\\frac{\\sin x}{x}-1}{\\frac{e^x-1}{x}-1}=0"
        );
        assert_eq!(
            render_latex(
                "\\frac{1+\\frac{1}{1+\\frac{1}{x}}}{1-\\frac{1}{1-\\frac{1}{x}}}",
                true
            )
            .as_deref(),
            Some("1+1/(1+1/x)\n───────────\n1-1/(1-1/x)"),
            "source: {:?}",
            "\\frac{1+\\frac{1}{1+\\frac{1}{x}}}{1-\\frac{1}{1-\\frac{1}{x}}}"
        );
    }

    /// Port of the upstream `it("keeps fractions linear in scripts and text-style fractions")`.
    #[test]
    fn keeps_fractions_linear_in_scripts_and_text_style_fractions() {
        assert_eq!(
            render_latex("e^{\\frac{1}{2}}", true).as_deref(),
            Some("e^(1/2)"),
            "source: {:?}",
            "e^{\\frac{1}{2}}"
        );
        assert_eq!(
            render_latex("\\tfrac{1}{2}", true).as_deref(),
            Some("1/2"),
            "source: {:?}",
            "\\tfrac{1}{2}"
        );
    }

    /// Port of the upstream `it("returns undefined for unsupported commands")`.
    #[test]
    fn returns_undefined_for_unsupported_commands() {
        assert_eq!(
            render("x + \\unknown{y}").as_deref(),
            None,
            "source: {:?}",
            "x + \\unknown{y}"
        );
    }

    /// Port of the upstream `it("returns undefined for malformed groups and environments")`.
    #[test]
    fn returns_undefined_for_malformed_groups_and_environments() {
        assert_eq!(
            render("\\frac{1}{x").as_deref(),
            None,
            "source: {:?}",
            "\\frac{1}{x"
        );
        assert_eq!(render("x}").as_deref(), None, "source: {:?}", "x}");
        assert_eq!(
            render("\\begin{matrix}1 & 2").as_deref(),
            None,
            "source: {:?}",
            "\\begin{matrix}1 & 2"
        );
        assert_eq!(render("x\\").as_deref(), None, "source: {:?}", "x\\");
    }
}
