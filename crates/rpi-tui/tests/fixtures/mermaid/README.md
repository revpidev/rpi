# Mermaid golden fixtures

Three-way golden check data for `rpi_tui::mermaid::render` (the grok-build
`crates/codegen/xai-grok-markdown/src/mermaid.rs` port, pinned commit noted in
`crates/rpi-tui/src/mermaid.rs`).

Each `*.txt` is the expected `plain_lines` output (joined with `\n`, trailing
newline), one byte per line, and each same-named `*.md` is the mermaid source
fed to the renderer.

Generated with `grok-mermaid` **0.2.2** (TypeScript, Apache-2.0), installed in
`external/pi/node_modules`:

- `flowchart.txt` / `sequence.txt` / `state.txt`: `render(src).plain`
- `unsupported_gantt.txt`: `sourceBox(src, 80).plain` — grok-mermaid's
  `render()` returns `null` for diagram kinds it does not draw, and leaves the
  boxed-source fallback to the caller; grok-build's `render()` boxes the
  source itself, which is the behavior asserted here.

Generator (run from the repo root, then move the files here):

```js
// /tmp/gen-golden.mjs
import { render, sourceBox } from './external/pi/node_modules/grok-mermaid/dist/index.js';
// ... per-case: art ? art.plain.join('\n') + '\n' : sourceBox(src, 80).plain.join('\n') + '\n'
```

grok-mermaid 0.2.2 is a newer independent port of grok-build and additionally
supports `classDiagram`/`erDiagram`; layout differences between it and the
pinned grok-build revision are recorded in
`crates/rpi-tui/tests/mermaid_golden.rs`.
