# Vendored locale tables

Byte-for-byte copies of upstream
`juicesharp/rpiv-mono` → `packages/rpiv-ask-user-question/locales/*.json`
@ `338b264c1ca4fd8828cc849b632f4f7ad88d2e78` (v2.9.0 + 7 commits).

- 9 locales: `de`, `en`, `es`, `fr`, `pt`, `pt-BR`, `ru`, `uk`, `zh`.
- `en.json` is the complete key set (26 keys) and the fallback base; the
  other locales are partial overlays merged over English at load
  (`zh.json` additionally carries a `_meta.notes` translator note, which is
  not a UI string).
- `i18n.rs` embeds each file with `include_str!`; no runtime file reads.
- `scripts/ask-user-question-parity/run-parity.mjs` re-checks every file's
  sha256 against the pinned submodule on each run (report section `locales`),
  so drift is caught mechanically.

To refresh after a future upstream pin bump: copy the files, then re-run the
parity harness.
