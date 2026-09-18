# MCP adapter cross-implementation parity harness (design §5.2)

The same fixture MCP server drives both client sides — the pinned upstream Node `McpServerManager`
(`rpi/external/pi-mcp-adapter` @ `10a45367` (v2.32.1, the baseline after the TE27 pin switch), read-only) and this crate's Rust
manager — and diffs the normalized frame sequences and result JSON. Any difference is attributable to the client
implementation itself (the fixture server is byte-identically constructed on both sides).

## Running

```bash
bash scripts/mcp-parity/run-parity-suite.sh         # one-shot: dependency install + four legs + archiving
bash scripts/mcp-parity/run-parity-suite.sh mcp-parity   # single leg (multi-selectable)
node scripts/mcp-parity/run-mcp-parity.mjs     # all scenarios; non-zero exit = differences exist
```

- Dependencies install into `/tmp/rpi-mcp-parity-deps` (`setup-deps.sh` copies the upstream
  `package.json` + `package-lock.json` then runs `npm ci` — the full transitive closure is pinned
  to the versions upstream tested; includes tsx and the official conformance referee).
  **Never writes into `rpi/external/`**.
- Reports land in `rpi/fixtures/generated/mcp-parity/` (`parity-report.md` +
  per-scenario `parity-<scenario>-{upstream,rpi}.json`), **committed to git as the
  evidence chain** (normalization strips run-time-volatile values, so re-runs produce no churn).

## Target track (TE13 skeleton, ADR-0025)

The default = the submodule worktree (after the TE27 pin switch = `10a45367` / v2.32.1, i.e. the rebase target baseline). The old-pin regression track (`3d953f90` / v2.24.0) reached end-of-life with TE27; for archaeological re-runs, point the upstream root at an old-pin worktree
snapshot outside the repository (zero writes to `external/`):

```bash
bash scripts/mcp-parity/setup-target-source.sh   # git archive v2.32.1 + npm ci from its lockfile
export RPI_MCP_PARITY_UPSTREAM=/tmp/rpi-mcp-parity-target-v2321
export RPI_MCP_PARITY_DEPS=/tmp/rpi-mcp-parity-target-v2321
node scripts/mcp-parity/run-mcp-parity.mjs --out-dir /tmp/mcp-target-parity
```

The three drivers (`run-mcp-parity.mjs` / `run-oauth-parity.mjs` / `render-call-upstream.mjs`)
all read `RPI_MCP_PARITY_UPSTREAM`, defaulting to the old pin, so the regression track behaves byte-for-byte unchanged. The target track's
golden/conformance re-record list and owning tasks are in [`TARGET-TRACK.md`](./TARGET-TRACK.md)
(naming goldens → TE23; conformance/pure-function vectors → TE24); TE13 delivered only the skeleton, no re-records.

## Scenarios

| Scenario | Transport | Fixture behavior |
| --- | --- | --- |
| `stdio` | Node fixture server subprocess stdin/stdout | Full functionality (initialize/tools/resources/prompts/call) |
| `http-streamable` | HTTP POST JSON | `Mcp-Session-Id` sessions |
| `http-fallback-404/405/406/415` | POST first answers a failure code → legacy SSE fallback | GET event stream + POST /message |
| `http-auth-401` | 401 + `WWW-Authenticate` | the needs-auth path |

## Normalization and exemptions (diff stability)

- JSON-RPC `id` → `$id` (each side increments from 0; same values but treated as an immutable sequence).
- `clientInfo.name` → `parity-client`: upstream `pi-mcp-<server>` vs rpi
  `rpi-mcp-<server>` (design §6 O1 branding-copy exemption).
- Discovery frame-order exemption: upstream issues tools/resources/prompts
  list concurrently via `Promise.all` (server-manager.ts:458-462), so arrival order jitters with
  scheduling (observed to differ run to run); rpi issues sequentially. JSON-RPC list requests are
  independent of each other, and the on-the-wire equivalent is the frame **set**, so consecutive
  discovery frames are sorted by method at comparison time while the on-disk transcript keeps the original order.
- `http-auth-401` expected difference: upstream's 401 handling enters the OAuth discovery flow inside
  connect (this stub's resource_metadata points at an unreachable port → error); the P0 rpi side stops at the
  needs-auth connection state (FR-P0-08 scope). Parity of the OAuth continuation belongs to TE03.

## renderCall pure-function parity (TE09 FR-E)

```bash
cargo build -p rpi-ext-mcp-adapter --example render_call_parity
node scripts/mcp-parity/run-render-call-parity.mjs
```

- Shared cases in `render-call-fixtures.json` (19 across proxy/direct/render,
  mirroring upstream `__tests__/tool-result-renderer.test.ts` samples + edge cases).
- The upstream leg is `render-call-upstream.mjs` (tsx running the pinned exported functions directly);
  `render-call-hooks.mjs` maps `@earendil-works/pi-tui` to a minimal-value stub
  (`render-call-host-pi-tui.mjs`) — that file makes **value imports** from pi-tui
  (constructing `Text`), so the protocol leg's throwing stub doesn't apply; the stub's `render` only
  splits on newlines (width wrapping is pi-tui's rendering responsibility and outside the parity
  surface; the cases stay under 80 columns).
- The rpi leg is `examples/render_call_parity.rs` (reads the same fixtures; render cases
  extract the component tree's text lines). Outputs:
  `fixtures/generated/mcp-parity/render-call-parity-{upstream,rpi}.json` +
  `render-call-parity.md`, committed to git as the evidence chain.
- This leg caught and fixed a JS truthiness difference on first run (upstream's empty
  `args:{}` object always outputs the `"{}"` summary line).

## Components

- `fixture-server.mjs`: the shared fixture server (stdio / http × 4 profiles), transcribing frames
  to `RPI_MCP_FIXTURE_LOG` (`RPI_MCP_FIXTURE_LOG_FRAMES=1` records full frames).
- `upstream-runner.mjs`: the upstream-side driver (tsx running the pinned TS sources directly).
- `mcp_adapter_parity_runner.rs` (crate example): the rpi-side driver with the same step sequence (P2-9 unique naming).
- `parity-hooks.mjs`: bare-dependency resolution into the external directory + host-package stubs (upstream
  makes type-only imports; the value import `complete` from `@earendil-works/pi-ai/compat` gets a
  throwing stub — no sampling is registered in parity scenarios).
- `setup-deps.sh`: external dependency installation (copies the upstream lockfile then `npm ci`; the full
  transitive closure matches upstream).
- `run-parity-suite.sh`: the one-shot re-run entry (dependency install → four legs → archiving); includes
  `conformance-baseline.yml` (expected failures) and
  `normalize-conformance.mjs` (conformance archive normalization: timestamps/ephemeral
  ports/session ids/retry jitter → markers).

## OAuth parity (TE02 self-test item 5 / TE03 prerequisite)

`run-oauth-parity.mjs` + `oauth-stub-server.mjs` +
`oauth-upstream-driver.mjs` + crate example `oauth_parity_runner.rs`:
a stub authorization server (RFC 8414 metadata + DCR + /authorize 302 +
/token) records request transcripts while both sides run the full authorization-code + PKCE flow
(upstream `mcp-auth-flow.ts startAuth/completeAuthFromInput`, rpi `oauth.rs
authenticate`), diffing the normalized DCR / authorization URL / token form parameters.

Normalization: `code_challenge`/`state`/`code_verifier`/`code` → markers; callback port →
`$port`; stub AS port → `$asport`; `client_name`/`client_uri` → `$client_name`
/`$client_uri` (O1 branding exemption); key-order-insensitive comparison (form/query
parameters are unordered).

```bash
node scripts/mcp-parity/run-oauth-parity.mjs   # → oauth-parity.md
```

## End-to-end five-mode parity (design §5.3)

`run-e2e-parity.mjs`: the same fixture config drives `pi -p` (upstream CLI +
the npm adapter) and `rpi -p` (this repository's CLI + native cdylib) through the five modes — list /
search / describe / call / status — diffing the normalized tool-result texts
(taken from the model reply's verbatim fenced block; model and periphery-copy differences are stripped).

Environment requirements and isolation: the upstream side points `PI_CODING_AGENT_DIR` at a temp agent dir
(fresh mcp-cache → both sides run bootstrap-all); the npm package directory is **copied** from the real HOME
(same policy as login state: read-only evidence into the sandbox — symlinks would let in-sandbox package
management write through to the real HOME), auth copied; the rpi side points `RPI_CODING_AGENT_DIR` at a
temp agent dir with `librpi_ext_mcp_adapter.so` + manifest installed, auth copied. Both sides need a usable
model-provider login.

```bash
node scripts/mcp-parity/run-e2e-parity.mjs   # → e2e-parity.md
```

## conformance Rust driver (O2 closeout)

The crate example `conformance_driver.rs` offers the same CLI contract as upstream's `conformance/driver.sh`
(`MCP_CONFORMANCE_SCENARIO=<scenario> driver <server-url>`),
driven over stdio by the official referee (`@modelcontextprotocol/conformance@0.1.16`, installed externally into
`/tmp/rpi-mcp-parity-deps` by setup-deps.sh alongside the upstream lockfile closure),
with the client under test being this crate's `McpServerManager`. Re-run entry:
`bash scripts/mcp-parity/run-parity-suite.sh conformance` (referee invocation +
the expected-failures baseline `conformance-baseline.yml` + stable-directory archiving +
normalization). The four core scenario results are archived under
`fixtures/generated/mcp-parity/conformance/` (`driver-summary.txt` +
a fixed directory per scenario `<scenario>/checks.json`, normalized and committed):

- `initialize`, `tools_call`, `sse-retry` PASS (sse-retry includes all three checks: retry: timing,
  Last-Event-ID, graceful reconnect)
- `elicitation-sep1034-client-defaults` FAIL (expected: server→client
  elicitation is P2 scope; P0 answers -32601)
- `auth/*` scenarios belong to TE03 (the OAuth matrix), reusing `baseline-client.yml` at that time

## Differences this harness caught and fixed

1. Missing `tools/list` capability guard: the SDK's `Client.listTools` sends no request and returns an empty
   list when the server doesn't declare the `tools` capability (console warning); rpi's thin client
   previously sent the request unconditionally. Aligned (protocol.rs `fetch_all_tools`).
2. Three probe-classification deviations (`mcp-probe.ts`): the 401 envelope lacked the Bearer-challenge
   check, the modern phase lacked the `unsupported-modern` fallback, and `responseKind` emitted an empty
   string for an empty content-type instead of "an untyped response". Aligned (manager.rs).
3. Missing SSE reconnect scheduling (SDK `_scheduleReconnection`/`_handleSseStream`):
   `retry:`-field-driven reconnect delays, `Last-Event-ID` replay headers, and GET reopen + response-id
   remapping when a per-request stream gets no response. Completed (protocol/http.rs);
   the conformance `sse-retry` scenario went from FAIL to PASS.
4. Four breakpoints in the OAuth authorization-code flow: the callback listener binding its port before
   constructing redirect_uri/DCR (previously hardcoded `localhost:0`), DCR redirect_uris sync,
   the DCR secret passed to token exchange, and the PKCE verifier read-back fallback. Fixed
   (oauth.rs); the stub-AS parity MATCHes.
