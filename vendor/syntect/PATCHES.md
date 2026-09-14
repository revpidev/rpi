# Vendored syntect fork

Upstream: [syntect](https://github.com/trishume/syntect) 5.3.0, vendored from the
crates.io registry archive (`.crate` sha256
`656b45c05d95a5704399aeef6bd0ddec7b2b3531b7c9e900abbf7c4d2190c925`, same
version the workspace pinned before vendoring). Wired in via
`[patch.crates-io]` in the workspace root `Cargo.toml`; behavior is identical to
the registry crate — the patches below only widen visibility so
`crates/rpi/build.rs` can filter the embedded syntax dump (issue
[revpidev/rpi#47](https://github.com/revpidev/rpi/issues/47)).

## Why a fork (and not upstream API)

`crates/rpi/build.rs` must rebuild the bat syntax set *without* the six
grammars whose regexes the pure-Rust fancy-regex backend cannot compile. A
linked dump cannot simply drop syntaxes: every `ContextReference::Direct` id
is an index into the old link, so **all** references must be rewritten to
resolvable ones (`Named` / `File`) before re-linking. That needs two things
upstream syntect keeps `pub(crate)` / `#[non_exhaustive]`:

1. `SyntaxSet::get_context` to resolve a `Direct` id back to its `Context`
   (for ownership: which syntax/context does this reference point into?);
2. constructing `ContextReference::Named` / `File` replacement references.

Precedent: broot maintains a syntect fork for the same class of problems
([sharkdp/bat#3156](https://github.com/sharkdp/bat/issues/3156) documents the
underlying panic; two-face's fancy dumps avoid it by filtering *before*
linking, which needs the grammar YAML sources syntect-assets does not ship).

## Patches

Exactly two, additive, no behavior change:

```diff
--- a/src/parsing/syntax_set.rs
+++ b/src/parsing/syntax_set.rs
@@ syntax_set.rs @@
-    pub(crate) fn get_context(&self, context_id: &ContextId) -> Result<&Context, ParsingError> {
+    pub fn get_context(&self, context_id: &ContextId) -> Result<&Context, ParsingError> {
```

```diff
--- a/src/parsing/syntax_definition.rs
+++ b/src/parsing/syntax_definition.rs
@@ pub enum ContextReference @@
-    #[non_exhaustive]
     Named(String),
-    #[non_exhaustive]
     ByScope {
-    #[non_exhaustive]
     File {
-    #[non_exhaustive]
     Inline(String),
-    #[non_exhaustive]
     Direct(ContextId),
```

(The enum itself keeps its `#[non_exhaustive]`; only the variant attributes
are dropped so the variants can be constructed from `build.rs`.)

## Trimmed relative to the registry archive

Not needed to build the library as a dependency (keeps the vendored tree at
~470 KB instead of 1.6 MB): `tests/`, `examples/`, `benches/`, `assets/`,
`Cargo.toml.orig`, `Cargo.lock`, `CHANGELOG.md`, `DESIGN.md`; the
corresponding `[[example]]`/`[[test]]/`[[bench]]`, `[dev-dependencies.*]` and
`[profile.*]` sections were removed from `Cargo.toml` (profile overrides in
dependency manifests are ignored by cargo anyway).

## Upgrading

When bumping syntect: re-vendor the new registry archive, re-apply the two
patches above (they are intentionally trivial), and re-trim as listed.
`crates/rpi/build.rs` then fails the build loudly if the new version's syntax
dump contains fancy-incompatible regexes outside `FANCY_INCOMPATIBLE`, or if
any rewritten reference cannot be resolved.
