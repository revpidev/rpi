# interactive-ui-parity (V14-22 C2 dual-carrier consistency harness)

The acceptance apparatus for R-U7.4 / G11 item 2: the same JSONL input script drives both the **native**
(`crates/rpi-test-native-plugin` cdylib) and **wasm**
(`examples/wasm-extension`) fixture guests, running the same
scripted component over the real host-call JSON channel
(`rpi-ext-host`'s `rpi_host_call` dispatch + `ScriptedUiBridge`), diffing `{lines,cursor?,done?}` frame by frame together with the terminal result.

## Running

```bash
# From the repository root (builds both fixtures automatically; falls back to the user-level rustup
# when the wasm32 target is missing)
python3 scripts/interactive-ui-parity/run.py

# Binary only (when the fixtures are already built)
cargo run -p rpi-test-support --bin interactive-ui-parity -- \
  --fixture scripts/interactive-ui-parity/corpus \
  --out fixtures/generated/interactive-ui-parity
```

Outputs:

- `fixtures/generated/interactive-ui-parity/parity-report.md` — the report (build/per-scenario/fuzz/conclusion);
- `<scenario>.native.json` / `<scenario>.wasm.json` — each carrier's complete transcript (frames / terminal /
  toolResult / mountOptions);
- `<scenario>.diff.json` — the consistency verdict (parity projection + documented-constraint markers).

A non-zero exit code = any frame-sequence/terminal-result difference, a missing fixture, or a load failure.

## Corpus format (`corpus/*.jsonl`)

One host event per line (in delivery order); the file name is the scenario name:

| Line | Event |
|---|---|
| `{"input":"a"}` | `input` (raw key bytes, not normalized) |
| `{"resize":[80,24]}` | `resize` |
| `{"tick":1}` | `tick` |
| `{"hidden":true}` / `{"hidden":false}` | `visibility` |
| `{"wake":1}` | `render` |
| `{"theme":{"name":"light"}}` | `theme` |
| `{"focus":true}` / `{"blur":true}` | `focus` / `blur` |
| `{"dispose":"sessionReload"}` | `dispose` (an exit path) |

Every scenario must end with an input that triggers `done` (`q`) or a `dispose`; a script that runs out early is judged a
failure by the harness (`scriptExhausted`).

> The driver copies the fixture packages under the system temp directory (cleaned per scenario); if `/tmp` is tight,
> set `TMPDIR=<a large directory>` before running (e.g. `TMPDIR=$HOME/.cache/rpi-ui-parity-tmp`).

## fuzz (§4.5)

`run.py` appends 24 random event sequences with a fixed seed (`20260910`) by default (lengths/interleavings/
hidden state/themes/focus/tick/wake/wide characters), likewise diffed across both carriers; any difference is a non-zero
exit, reproducible with `--fuzz N --seed S`.

## Allowed differences (documented execution constraints)

Only the constraints already established in design §4.4 may appear in the comparison, and the harness marks them explicitly:

- **wasm frame budget**: `mountOptions.maxFrameBytes` is clamped to 512 KiB by the wasm carrier
  (native keeps the guest's requested value). This is not a behavioral difference; `documented_constraint_only`
  verifies that "the two transcripts are otherwise completely equal" before ruling MATCH.

Any other frame-sequence/terminal-result difference is a failure (R-U7.4). Host-side registry behavior (tick
pause/resume, wake thread safety, limits, fuel/trap unloading) is covered by the
unit tests of `crates/rpi` / `crates/rpi-ext-host`, not duplicated in this harness.
