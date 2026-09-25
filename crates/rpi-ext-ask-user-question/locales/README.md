# Vendored locale tables

Byte-for-byte copies of upstream
`juicesharp/rpiv-mono` → `packages/rpiv-ask-user-question/locales/*.json`
@ `0fdf4f813980d380e826b84d1280a4960e5d088e` (v2.10.1+1; the 338b264..0fdf4f8 span is comment-level for the locales (zero diff)).

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
