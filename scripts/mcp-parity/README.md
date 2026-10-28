# Built-in MCP parity harness (V16-08 G3)

> The five adapter legs (mcp-parity / render-call-parity / oauth-parity /
> conformance / e2e-parity) were removed together with
> `crates/rpi-ext-mcp-adapter` (ADR-0037, TE49) — the adapter was their
> upstream/rpi driver source. This directory now carries the single
> remaining leg plus the shared fixture server.

The built-in `crates/rpi-mcp` client is compared against the upstream package
client `external/pi/packages/mcp` @ `abe508e1b` (v1.1.0, read-only). The
shared fixture server (`fixture-server.mjs`, stdio) drives both sides, and
the normalized transcripts/results are diffed:

- rpi side: `crates/rpi-mcp/examples/builtin_mcp_parity_runner.rs`
  (`McpClient` + `StdioTransport`)
- upstream side: `builtin-upstream-runner.mjs` (tsx running the pinned
  `packages/mcp` `src/index.ts` directly)

Coverage: `initialize` handshake, `notifications/initialized`, `tools/list`,
`tools/call` (success and error result), `resources/read`. Normalization: ids
→ `$id`, `clientInfo.name` → `parity-client` (O1 brand exemption).

## Running

```bash
bash scripts/mcp-parity/run-parity-suite.sh         # one-shot: leg + verdict
node scripts/mcp-parity/run-builtin-mcp-parity.mjs  # leg directly
# → fixtures/generated/mcp-parity/builtin-parity-report.md
#   + builtin-parity-stdio-{upstream,rpi}.json (committed evidence chain)
```

The leg resolves `tsx` from `/tmp/rpi-mcp-parity-deps` when present, otherwise
from `external/pi/node_modules`, so it runs standalone without any out-of-tree
dependency closure beyond an npm install that contains tsx:

- `RPI_MCP_PARITY_DEPS` — out-of-tree npm install root (default
  `/tmp/rpi-mcp-parity-deps`); tsx may instead be resolved from
  `external/pi/node_modules`.
- `RPI_MCP_PARITY_UPSTREAM` — upstream pi source root (default `external/pi`).
- `RPI_MCP_PARITY_CARGO` — cargo binary (default `cargo`).
- `--out-dir <dir>` — report directory override (default
  `fixtures/generated/mcp-parity/`).

Reports land in `rpi/fixtures/generated/mcp-parity/` and are **committed to
git as the evidence chain** (normalization strips run-time-volatile values, so
re-runs produce no churn).

## Components

- `fixture-server.mjs`: the shared fixture server (stdio / http × 4 profiles),
  transcribing frames to `RPI_MCP_FIXTURE_LOG`
  (`RPI_MCP_FIXTURE_LOG_FRAMES=1` records full frames).
- `builtin-upstream-runner.mjs`: the upstream-side driver (tsx running the
  pinned `packages/mcp` sources directly).
- `run-builtin-mcp-parity.mjs`: the orchestrator (builds the rpi example,
  drives both sides, normalizes, diffs, writes the report).
- `run-parity-suite.sh`: the one-shot re-run entry (leg → summary → exit
  code).