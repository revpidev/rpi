//! Generates the embedded syntax-highlighting dump for `core/highlight.rs`
//! (T17-W2, ADR-0008 / D-051).
//!
//! The embedded syntax set is bat's curated set (`syntect-assets`, 198
//! syntaxes), **rebuilt without the six grammars whose regexes the pure-Rust
//! fancy-regex backend cannot compile** and re-serialized compressed with our
//! pinned syntect version (`syntect::dumps::dump_binary`, ~800KB), so the
//! binary embeds one self-contained blob instead of depending on syntect's
//! built-in defaults (which, as of syntect 5.3, lack TypeScript/TOML/
//! Dockerfile/…).
//!
//! Why removal instead of the previous name-only exclusion (issue #47):
//! syntect compiles grammar regexes lazily and panics on one it cannot
//! compile (`syntect/src/parsing/regex.rs`). The old runtime check only
//! stopped *lookup by name* — other grammars reach the excluded grammars'
//! contexts through cross-syntax references (`include: scope:source.js`
//! resolves `JavaScript (Babel)` over `JavaScript`, because syntect's linker
//! picks the last syntax owning a scope), so rendering HTML with a
//! `<script>` block, an Elixir `~r` sigil etc. panicked on Babel's
//! `\g<-1>` subroutine-call regexes. Removing a syntax from a *linked* dump
//! is not enough on its own: every `ContextReference::Direct` is an index
//! into the old link, so this script rewrites **all** of them to resolvable
//! references first (same-syntax → `Named`, cross-syntax → `File` by name;
//! references into a removed grammar rebind to another kept syntax owning
//! the same scope — `source.js` falls back to plain `JavaScript`; where no
//! such syntax exists the reference stays unresolved and the runtime parser
//! degrades that construct to plain text) and then re-links. That rewrite
//! needs the two additive visibility patches of the vendored syntect fork
//! (`vendor/syntect/PATCHES.md`).
//!
//! Gates (fail the build): the `FANCY_INCOMPATIBLE` list must exactly match
//! the syntaxes present in the asset set, syntax names must be unique (the
//! `File`-by-name rewrite relies on it), and after filtering **zero** regex
//! may fail to compile against the fancy-regex backend.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use syntect::dumps::{dump_binary, from_reader};
use syntect::parsing::syntax_definition::{
    Context, ContextReference, MatchOperation, Pattern, SyntaxDefinition,
};
use syntect::parsing::{Regex, Scope, SyntaxSet, SyntaxSetBuilder};
use syntect_assets::assets::HighlightingAssets;

/// Syntaxes whose regexes cannot be compiled by the pure-Rust fancy-regex
/// backend used by syntect 5.3 (verified empirically against syntect-assets
/// 0.23.6): subroutine-call syntax (`\g<...>`), `\p{Print}` and
/// variable-length look-behind are not implemented by fancy-regex. Removed
/// from the embedded dump by this script; `core/highlight.rs` keeps a mirror
/// of the list so a build.rs regression still cannot resolve them by name.
/// This matches the exclusion list curated by the `two-face` project for its
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
    // download guidance when it's absent (non-cargo builds); never guess.
    let target = std::env::var("TARGET").unwrap_or_else(|_| panic!("no TARGET"));
    println!("cargo:rustc-env=RPI_BUILD_TARGET={target}");

    // bat's curated set (embedded in syntect-assets as an uncompressed dump).
    let assets = HighlightingAssets::from_binary();
    let full = assets
        .get_syntax_set()
        .unwrap_or_else(|e| panic!("load syntect-assets syntax set: {e}"));
    let dump = dump_binary(full);

    // Two independent deserializations of the same dump: `walk` resolves
    // `ContextId` → `Context` (the vendored fork exposes `get_context`) for
    // ownership lookups; the builder names every context. Like the previous
    // pipeline, the round-trip also re-indexes context references so the
    // definitions are self-consistent.
    let walk: SyntaxSet =
        from_reader(&dump[..]).unwrap_or_else(|e| panic!("round-trip syntax set dump: {e}"));
    let owned: SyntaxSet =
        from_reader(&dump[..]).unwrap_or_else(|e| panic!("round-trip syntax set dump: {e}"));
    let builder: SyntaxSetBuilder = owned.into_builder();

    let syntaxes = builder.syntaxes();
    let total = syntaxes.len();

    // Gate: the exclusion list must exactly cover syntaxes that exist.
    for name in FANCY_INCOMPATIBLE {
        assert!(
            syntaxes.iter().any(|def| &def.name == name),
            "FANCY_INCOMPATIBLE lists syntax {name:?} which is absent from the \
             syntect-assets set — refresh the list (upstream set changed?)"
        );
    }
    // Gate: `File`-by-name rewriting below assumes unique syntax names.
    let mut names: HashSet<&str> = HashSet::with_capacity(total);
    for def in syntaxes {
        assert!(
            names.insert(def.name.as_str()),
            "syntax name {:?} is not unique in the syntect-assets set — the \
             File-by-name rewrite would be ambiguous",
            def.name
        );
    }

    let owners = OwnerIndex::build(syntaxes);
    // (name, scope, excluded) in asset order — used to rebind references
    // that pointed into a removed grammar.
    let syntax_table: Vec<(String, Scope, bool)> = syntaxes
        .iter()
        .map(|def| {
            (
                def.name.clone(),
                def.scope,
                FANCY_INCOMPATIBLE.contains(&def.name.as_str()),
            )
        })
        .collect();

    // Rewrite every Direct reference of every kept syntax, then drop the
    // excluded ones by never re-adding them.
    let mut stats = RewriteStats::default();
    let mut rewritten: Vec<SyntaxDefinition> = Vec::with_capacity(total);
    for def in syntaxes {
        if FANCY_INCOMPATIBLE.contains(&def.name.as_str()) {
            continue;
        }
        let mut def = def.clone();
        rewrite_direct_refs(&mut def, &walk, &owners, &syntax_table, &mut stats);
        rewritten.push(def);
    }

    // Gate: after filtering, every regex of every syntax must compile against
    // the fancy-regex backend (previously this gate allowed the excluded six;
    // they are gone now, so any failure is a regression or an asset change).
    let mut test_builder = SyntaxSetBuilder::new();
    for def in &rewritten {
        test_builder.add(def.clone());
    }
    let failures = fancy_compile_failures(&test_builder);
    if !failures.is_empty() {
        panic!(
            "regexes that the fancy-regex backend cannot compile survived \
             filtering (new incompatible grammar or rewrite bug?):\n{}",
            failures
                .iter()
                .map(|(name, detail)| format!("{name}: {detail}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let mut nb = SyntaxSetBuilder::new();
    for def in rewritten {
        nb.add(def);
    }
    let filtered = nb.build();

    let expected = total - FANCY_INCOMPATIBLE.len();
    assert_eq!(
        filtered.syntaxes().len(),
        expected,
        "filtered syntax set must keep exactly {expected} of {total} syntaxes"
    );
    // Round-trip sanity: the shipped blob must deserialize to the same set.
    let reloaded: SyntaxSet = from_reader(&dump_binary(&filtered)[..])
        .unwrap_or_else(|e| panic!("round-trip filtered dump: {e}"));
    assert_eq!(reloaded.syntaxes().len(), expected);

    println!(
        "syntax dump: {total} -> {expected} syntaxes, {} refs rewritten \
         ({} same-syntax Named, {} cross-syntax File, {} rebound to a \
         same-scope kept syntax, {} left unresolved for graceful runtime \
         fallback)",
        stats.total(),
        stats.intra_named,
        stats.cross_named,
        stats.rebound,
        stats.unresolved
    );

    let out_dir =
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_else(|| panic!("no OUT_DIR")));
    fs::write(out_dir.join("syntaxes.bin"), dump_binary(&filtered))
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

// ===========================================================================
// Direct-reference rewrite (issue #47)
// ===========================================================================

#[derive(Default)]
struct RewriteStats {
    intra_named: usize,
    cross_named: usize,
    /// References into a removed grammar, rebound to a kept grammar owning
    /// the same scope (e.g. `JavaScript (Babel)` → `JavaScript`).
    rebound: usize,
    /// References into a removed grammar with no same-scope replacement;
    /// left as unresolvable `File` references — pushing them at parse time
    /// returns `ParsingError::UnresolvedContextReference`, which the
    /// highlighter treats as "no highlight" (plain-text fallback), never a
    /// panic.
    unresolved: usize,
}

impl RewriteStats {
    fn total(&self) -> usize {
        self.intra_named + self.cross_named + self.rebound + self.unresolved
    }
}

/// Rewrite every `ContextReference::Direct` in `def` to a resolvable
/// reference. `walk` resolves the old id to its `Context`; `owners` maps
/// that context back to (syntax, context-name) by structural equality;
/// `syntax_table` drives the same-scope rebinding for removed grammars.
/// `Context.prototype` links are regenerated by `SyntaxSetBuilder::build`
/// and need no handling here.
fn rewrite_direct_refs(
    def: &mut SyntaxDefinition,
    walk: &SyntaxSet,
    owners: &OwnerIndex,
    syntax_table: &[(String, Scope, bool)],
    stats: &mut RewriteStats,
) {
    let def_name = def.name.clone();
    let mut rewrite = |r: &mut ContextReference| {
        let ContextReference::Direct(id) = *r else {
            return;
        };
        let Some((owner_syntax, owner_ctx)) =
            walk.get_context(&id).ok().and_then(|c| owners.find(c))
        else {
            // Ownership resolution failure would leave a stale Direct id
            // behind — better to fail the build than corrupt the dump.
            panic!(
                "cannot resolve context reference of syntax {def_name:?} — \
                 dump layout changed?"
            );
        };
        if *owner_syntax == def_name {
            *r = ContextReference::Named(owner_ctx.to_string());
            stats.intra_named += 1;
            return;
        }
        let owner_excluded = syntax_table
            .iter()
            .find(|(name, _, _)| name == owner_syntax)
            .map(|&(_, _, excluded)| excluded)
            .unwrap_or_else(|| panic!("syntax {owner_syntax:?} missing from table"));
        if !owner_excluded {
            *r = ContextReference::File {
                name: owner_syntax.to_string(),
                sub_context: Some(owner_ctx.to_string()),
                with_escape: false,
            };
            stats.cross_named += 1;
            return;
        }
        // Reference into a removed grammar: rebind to the last kept syntax
        // owning the same scope (syntect's own linker picks the last owner,
        // so this matches what a from-scratch link over the kept set would
        // choose), targeting its main context.
        let owner_scope = syntax_table
            .iter()
            .find(|(name, _, _)| name == owner_syntax)
            .map(|&(_, scope, _)| scope)
            .expect("owner in table");
        let replacement = syntax_table
            .iter()
            .rev()
            .find(|(_, scope, excluded)| *scope == owner_scope && !excluded)
            .map(|(name, _, _)| name.clone());
        match replacement {
            Some(name) => {
                *r = ContextReference::File {
                    name,
                    sub_context: None,
                    with_escape: false,
                };
                stats.rebound += 1;
            }
            None => {
                *r = ContextReference::File {
                    name: owner_syntax.to_string(),
                    sub_context: Some(owner_ctx.to_string()),
                    with_escape: false,
                };
                stats.unresolved += 1;
            }
        }
    };
    for ctx in def.contexts.values_mut() {
        for pat in &mut ctx.patterns {
            match pat {
                Pattern::Include(r) => rewrite(r),
                Pattern::Match(mp) => {
                    if let Some(r) = &mut mp.with_prototype {
                        rewrite(r);
                    }
                    match &mut mp.operation {
                        MatchOperation::Push(rs) | MatchOperation::Set(rs) => {
                            for r in rs {
                                rewrite(r);
                            }
                        }
                        MatchOperation::Pop | MatchOperation::None => {}
                    }
                }
            }
        }
    }
}

type Db = Vec<(String, String, Context)>; // (syntax, context-name, context)

/// Maps contexts of the loaded dump back to their (syntax, context-name)
/// owners by structural equality. Bucketed by a cheap signature first so the
/// ~21k reference resolutions stay fast; collision buckets fall back to full
/// `Context` equality (all fields compare by value across two deserializations
/// of the same dump — `Regex` compares by pattern string, `Scope` by atom id).
struct OwnerIndex {
    db: Db,
    buckets: HashMap<(usize, usize, usize, bool, bool), Vec<usize>>,
}

impl OwnerIndex {
    fn build(syntaxes: &[SyntaxDefinition]) -> OwnerIndex {
        let mut db: Db = Vec::new();
        for def in syntaxes {
            for (name, ctx) in &def.contexts {
                db.push((def.name.clone(), name.clone(), ctx.clone()));
            }
        }
        let mut buckets: HashMap<(usize, usize, usize, bool, bool), Vec<usize>> = HashMap::new();
        for (i, (_, _, ctx)) in db.iter().enumerate() {
            buckets.entry(sig(ctx)).or_default().push(i);
        }
        OwnerIndex { db, buckets }
    }

    fn find(&self, ctx: &Context) -> Option<(&str, &str)> {
        let bucket = self.buckets.get(&sig(ctx))?;
        for &i in bucket {
            if self.db[i].2 == *ctx {
                return Some((&self.db[i].0, &self.db[i].1));
            }
        }
        None
    }
}

fn sig(ctx: &Context) -> (usize, usize, usize, bool, bool) {
    (
        ctx.patterns.len(),
        ctx.meta_scope.len(),
        ctx.meta_content_scope.len(),
        ctx.uses_backrefs,
        ctx.prototype.is_some(),
    )
}
