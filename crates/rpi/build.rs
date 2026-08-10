//! Generates the embedded syntax-highlighting dump for `core/highlight.rs`
//! (T17-W2, ADR-0008 / D-051).
//!
//! The embedded syntax set is bat's curated set (`syntect-assets`, 198
//! syntaxes) re-serialized with our pinned syntect version and zlib-compressed
//! (`syntect::dumps::dump_binary`, ~800KB), so the binary embeds one
//! self-contained blob instead of depending on syntect's built-in defaults
//! (which, as of syntect 5.3, lack TypeScript/TOML/Dockerfile/…).
//!
//! Every regex of every syntax is re-compiled against the fancy-regex backend
//! below. syntect compiles grammar regexes lazily and panics on one it cannot
//! compile (`syntect/src/parsing/regex.rs`), so a syntax failing this check
//! must be added to [`FANCY_INCOMPATIBLE`] and is then excluded by name at
//! runtime (`core/highlight.rs`); any *other* failure aborts the build.

use std::fs;
use std::path::PathBuf;

use syntect::dumps::{dump_binary, from_reader};
use syntect::parsing::syntax_definition::Pattern;
use syntect::parsing::{Regex, SyntaxSet, SyntaxSetBuilder};
use syntect_assets::assets::HighlightingAssets;

/// Syntaxes whose regexes cannot be compiled by the pure-Rust fancy-regex
/// backend used by syntect 5.3 (verified empirically against syntect-assets
/// 0.23.6): subroutine-call syntax (`\g<...>`), `\p{Print}` and
/// variable-length look-behind are not implemented by fancy-regex. This
/// matches the exclusion list curated by the `two-face` project for its
/// fancy-regex dumps (PowerShell / ARM Assembly / JavaScript (Babel) / Salt
/// State (SLS)), plus two further syntaxes its (different) source set does
/// not contain (Regular Expressions (Elixir) / VimHelp).
///
/// Impact on the upstream 43-language extension table (theme.ts:1188-1247):
/// only `powershell` (`.ps1`) is affected — it falls back to the plain
/// `mdCodeBlock` color, exactly like any unsupported language
/// (theme.ts:1162-1168).
const FANCY_INCOMPATIBLE: &[&str] = &[
    "ARM Assembly",
    "JavaScript (Babel)",
    "PowerShell",
    "Regular Expressions (Elixir)",
    "Salt State (SLS)",
    "VimHelp",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // T18 (ADR-0011 §4): inject the build target triple so the binary
    // self-updater can name its release asset — glibc vs musl is not
    // distinguishable at runtime via `std::env::consts`. Consumers read it
    // with `option_env!("RPI_BUILD_TARGET")` and fall back to manual
    // download guidance when it is absent (non-cargo builds); never guess.
    let target = std::env::var("TARGET").unwrap_or_else(|_| panic!("no TARGET"));
    println!("cargo:rustc-env=RPI_BUILD_TARGET={target}");

    // bat's curated set (embedded in syntect-assets as an uncompressed dump).
    let assets = HighlightingAssets::from_binary();
    let full = assets
        .get_syntax_set()
        .unwrap_or_else(|e| panic!("load syntect-assets syntax set: {e}"));

    // Owned copy for `into_builder()` (`SyntaxSet` is not `Clone`); the
    // round-trip also re-indexes context references so the definitions are
    // self-consistent.
    let owned: SyntaxSet = from_reader(&dump_binary(full)[..])
        .unwrap_or_else(|e| panic!("round-trip syntax set dump: {e}"));
    let builder: SyntaxSetBuilder = owned.into_builder();
    let failures = fancy_compile_failures(&builder);
    let unexpected: Vec<String> = failures
        .iter()
        .filter(|(name, _)| !FANCY_INCOMPATIBLE.contains(&name.as_str()))
        .map(|(name, detail)| format!("{name}: {detail}"))
        .collect();
    if !unexpected.is_empty() {
        panic!(
            "syntaxes incompatible with the fancy-regex backend were found outside \
             FANCY_INCOMPATIBLE (add them or fix them):\n{}",
            unexpected.join("\n")
        );
    }

    let out_dir =
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_else(|| panic!("no OUT_DIR")));
    fs::write(out_dir.join("syntaxes.bin"), dump_binary(full))
        .unwrap_or_else(|e| panic!("write syntaxes.bin: {e}"));
}

/// Re-compile every regex of every syntax with the fancy-regex backend.
/// Patterns with capture groups are compiled at runtime only after syntect
/// substitutes back-reference placeholders (`\N`) with matched text, so they
/// are checked with dummy placeholders instead. Returns `(syntax, detail)`
/// pairs for every failure.
fn fancy_compile_failures(builder: &SyntaxSetBuilder) -> Vec<(String, String)> {
    let mut failures: Vec<(String, String)> = Vec::new();
    for def in builder.syntaxes() {
        for (context, ctx) in &def.contexts {
            for pat in &ctx.patterns {
                let Pattern::Match(mp) = pat else {
                    continue;
                };
                let pattern = mp.regex().regex_str();
                let compiled = if mp.has_captures {
                    substitute_dummy_backrefs(pattern)
                } else {
                    pattern.to_string()
                };
                if let Some(err) = Regex::try_compile(&compiled) {
                    failures.push((
                        def.name.clone(),
                        format!("{context}: {:?} ({err})", compiled),
                    ));
                }
            }
        }
    }
    failures
}

/// Replace `\1`..`\9` back-reference placeholders with a literal, mimicking
/// syntect's runtime `substitute_backrefs_in_regex` (syntax_definition.rs).
fn substitute_dummy_backrefs(pattern: &str) -> String {
    let mut out = pattern.to_string();
    for i in 1..=9 {
        out = out.replace(&format!("\\{i}"), "x");
    }
    out
}
