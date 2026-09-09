# ask-user-question parity harness (TE28 G3/G12)

Drives the **pinned upstream pure-function modules** (`juicesharp/rpiv-mono` →
`packages/rpiv-ask-user-question/` @ `338b264c` = v2.9.0 + 7 commits, the #192
line-terminator fix) against the Rust port in `crates/rpi-ext-ask-user-question`
and diffs the two legs case by case. **Non-zero exit = any difference.**

```bash
# one-time: the orchestrator installs tsx + typebox into the deps dir
# (override with RPI_ASKQ_PARITY_DEPS; default /tmp/rpi-ask-user-question-parity-deps)
cargo build -p rpi-ext-ask-user-question --example parity_runner
node scripts/ask-user-question-parity/run-parity.mjs
```

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
`state/row-intent.ts` (type-only imports into `view/` are erased by tsx).

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
