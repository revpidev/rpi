# ask-user-question parity harness (TE28/TE29 G3/G12)

Drives the **pinned upstream pure-function modules** (`juicesharp/rpiv-mono` →
`packages/rpiv-ask-user-question/` @ `338b264c` = v2.9.0 + 7 commits, the #192
line-terminator fix) against the Rust port in `crates/rpi-ext-ask-user-question`
and diffs the two legs case by case. **Non-zero exit = any difference.**

```bash
# one-time: the orchestrator installs tsx + typebox into the deps dir
# (override with RPI_ASKQ_PARITY_DEPS; default /tmp/rpi-ask-user-question-parity-deps)
node scripts/ask-user-question-parity/run-parity.mjs
```

The orchestrator builds this crate's `parity_runner` example itself before the
Rust leg: three workspace crates (ask-user-question / mcp-adapter / subagents)
ship an example with that name, so cargo's shared
`target/debug/examples/parity_runner` is whichever was built last. The in-script
build makes the harness independent of workspace build order (mcp-parity
precedent); the manual `cargo build -p rpi-ext-ask-user-question --example
parity_runner` step is therefore optional.

Report: `fixtures/generated/ask-user-question-parity/parity-report.md` (plus
`upstream-<group>.jsonl` / `rust-<group>.jsonl` raw legs).

## What runs

| File | Role |
|------|------|
| `fixtures.json` | Shared cases, grouped `schema` / `normalize` / `validate` / `envelope` / `row-intent` |
| `upstream-runner.mjs` | Imports the upstream TS modules via `tsx` and prints one JSON line per case |
| `run-parity.mjs` | Verifies the submodule pin, materializes the snapshot, runs both legs, normalizes, diffs, writes the report |
| `examples/parity_runner.rs` (in the crate) | Rust leg over the same fixtures via the `parity` facade |

## Snapshot policy (external/ stays read-only)

Node resolves bare specifiers (`typebox`) by walking up from the importing
file, so running the upstream modules **in place** would require writing a
`node_modules/` into the submodule. Instead `run-parity.mjs` copies the six
driven modules into `$RPI_ASKQ_PARITY_DEPS/snapshot/` (fresh every run) and
records each file's sha256 in the report. The deps dir carries `tsx` + the
`typebox` version from the upstream `package-lock.json` (1.3.6). The submodule
HEAD is asserted against `338b264c1ca4fd8828cc849b632f4f7ad88d2e78`.

Driven modules: `tool/{types,normalize-params,validate-questionnaire,response-envelope,format-answer}.ts`,
`state/row-intent.ts`, `state/i18n-bridge.ts` + `rpc-fallback.ts` (TE29 walker leg; type-only imports into
`view/` are erased by tsx; the i18n bridge falls back to its identity `t` when the `rpiv-i18n` SDK is
absent — the harness deps do not install it — so the walker leg runs on canonical English, and the Rust
leg pins `I18n::for_locale("en")` to match).

## Normalization whitelist

- **Key order**: both legs are deep-sorted before comparison (JS insertion
  order vs `serde_json` map order). Values, strings and array order are
  compared verbatim — there is no other normalization.
- **Locales**: the vendored `crates/rpi-ext-ask-user-question/locales/*.json`
  are compared **byte-for-byte** (sha256) against the submodule, with no
  whitelist.

## Fixture groups

- `schema` — full `QuestionParamsSchema` + constants + reserved/sentinel
  labels (canonical-JSON equality against live TypeBox output).
- `normalize` — CRLF / lone CR / mixed / astral / absent-vs-empty `preview` /
  `multiSelect` presence.
- `validate` — all six error codes, `reserved_label` before
  `duplicate_option_label`, duplicate-question precedence, case-sensitive
  reserved matching.
- `envelope` — option/custom/multi, `(no input)` placeholders, preview/notes
  suffixes, global note, cancel, partial submission, `null` result,
  out-of-order answers.
- `row-intent` — sentinel append matrix + the full `ROW_INTENT_META` /
  `LABELS_BY_KIND` / reserved-set constants.
- `rpc` (TE29) — `hasDialogUI` judgment table (`{select, input}` flags + `undefined` ui) and the
  sequential dialog walker: each case drives a scripted `DialogUI` (`{reply}` / `{cancel}` entries,
  exhausted script = dismiss) and compares the recorded calls (method/title/options/placeholder) and
  the final `QuestionnaireResult` verbatim — titles (header prefix, preview folding, ≤600 UTF-16-unit
  truncation incl. a clean astral boundary), option lines + sentinel row, multi-select token parsing
  (indices/dedup/space/period/out-of-range/empty commit/custom), cancels and mid-walk partial answers.
  The astral fixture pins a *clean* boundary on purpose — a cut that would split a surrogate pair
  cannot be byte-pinned (JS `slice` keeps a lone surrogate, which is invalid UTF-8 in Rust; the
  walker stops at the previous char instead, see `rpc_fallback.rs::truncate_utf16`).
