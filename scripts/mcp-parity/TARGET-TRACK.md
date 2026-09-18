# mcp-parity target-track skeleton and re-record list (delivered by TE13; the actual re-records belong to TE23/TE24)

> **The pin switched (TE27, 2026-09-11; ADR-0025 adopted)**: `external/pi-mcp-adapter` @
> `10a45367e033a32026987a75d6f401e37340c86f` (v2.32.1, 90 commits).
> This file is kept as the target-track historical record; the default driver (the submodule worktree) is already v2.32.1, and the snapshot path
> `/tmp/rpi-mcp-parity-target-v2321` serves only as an independent reference source.

## 1. What the skeleton delivered (TE13, complete)

| Item | Location | Notes |
|----|------|------|
| Switchable upstream root | `run-mcp-parity.mjs` / `run-oauth-parity.mjs` / `render-call-upstream.mjs` | All read `RPI_MCP_PARITY_UPSTREAM`, default = the submodule worktree (since TE27 = v2.32.1) |
| Target source/deps externalized | `setup-target-source.sh` | `git archive` extracts v2.32.1 into `/tmp/rpi-mcp-parity-target-v2321` and runs `npm ci` from its lockfile (zero writes to external/) |
| Conformance baseline regeneration entry | This file §3 + `run-parity-suite.sh conformance` | The baseline is the rpi client's expected-failures list (independent of the upstream tag; the regeneration entry was accepted with TE24) |
| New behavioral-parity surface list | This file §4 | Approval scoping/backoff visibility/503·202·401 classification/nested parameters/OAuth 401 (all landed and accepted by TE21/TE22) |

**What this skeleton deliberately does not do**: no default-driver switch, no `conformance-baseline.yml` re-record, no golden
vector re-records, no crate implementation changes — those belong to
TE23/TE24/TE21/TE22 respectively (the landing order of G10's "parity before implementation" is in
each task's document).

## 2. Running the target track (skeleton verification)

```bash
# One-time: external snapshot + its lockfile closure
bash scripts/mcp-parity/setup-target-source.sh
export RPI_MCP_PARITY_UPSTREAM=/tmp/rpi-mcp-parity-target-v2321
export RPI_MCP_PARITY_DEPS=/tmp/rpi-mcp-parity-target-v2321

# Protocol leg / renderCall leg (target-pin sources + target closure)
node scripts/mcp-parity/run-mcp-parity.mjs --out-dir /tmp/mcp-target-parity
node scripts/mcp-parity/run-render-call-parity.mjs
```

Until M1–M3 complete, the target track is **expected to produce differences** (new namespace
tools, request headers, backoff, naming, …);
the difference list is the acceptance entry for the §4 owning tasks. After TE23/TE24 complete their
batches, the target track must converge to
zero differences (goldens re-recorded against the new pin).

### TE13 skeleton field test (2026-09-08)

| Leg | Target-track result | Notes |
|----|------------|------|
| renderCall pure functions (24 cases) | **24/24 byte-identical** (exit 0) | v2.32.1's renderer newly imports `truncateToWidth`/`visibleWidth` at runtime; the stub was completed per pi-tui's printable-ASCII fast path (`render-call-host-pi-tui.mjs`), all cases being short ASCII lines |
| Protocol leg (7 scenarios) | 6 DIFF + `http-auth-401` MATCH (exit 1) | All DIFFs are v2.32.1-new surfaces (namespace tools/request headers, …), expected, converging after the TE23/TE24 re-records |

- The report-header pin is controlled by `RPI_MCP_PARITY_UPSTREAM_PIN` (default `3d953f90`; set `10a45367` for the target track),
  preventing target-track reports from mislabeling the old pin.
- Target-track output must use `--out-dir` / `RPI_MCP_PARITY_OUT_DIR` pointing at a scratch directory, never overwriting
  the regression evidence in `fixtures/generated/mcp-parity/`.
- **Stub limitations** (§4.3 input): the current pi-tui stub implements only the printable-ASCII fast path; if the TE24
  re-recorded render vectors contain ANSI/wide characters, map `@earendil-works/pi-tui` to the
  real package inside the target dependency root (the mapping point in `render-call-hooks.mjs`) and register it in the
  task document.

## 3. Re-record list: naming goldens (D-R6, owned by **TE23**; re-record before changing the implementation)

> **TE23 landing (2026-09-09)**: 3.1/3.2 were re-recorded via the target track (`RPI_MCP_FIXTURE_UPSTREAM` /
> `RPI_MCP_FIXTURE_PIN=10a45367` / `RPI_MCP_FIXTURE_ONLY=names,glob`); `glob_cases.json`
> was re-recorded in the same batch because the candidate-set semantics belong to FR-B
> (zero expectation-boolean changes, candidate lists extended).
> `golden_names.rs`/`golden_glob.rs` kept their assertion shapes, only gaining the new signature's `other_current_candidates` parameter.

| # | Object | Action | Basis |
|---|------|------|------|
| 3.1 | `crates/rpi-ext-mcp-adapter/tests/fixtures/name_format_cases.json` | **Done** (TE23): re-recorded against the v2.32.1 naming rules (BREAKING: server prefixes keep `-`/`_`; `a-b` is no longer encoded as `a_2d_b`) | R7.2.4, requirements appendix A |
| 3.2 | `crates/rpi-ext-mcp-adapter/tests/golden_names.rs` | **Done** (TE23): expectations driven by 3.1; "old expectation → new expectation + upstream commit" registered case by case in TE23 §7 (G2) | R7.2.4.4 |
| 3.3 | `changes/` callout | **Done** (TE23): a BREAKING entry + the `toolPrefix:"none"` migration guide; no dual old/new registration | G10 |

## 4. Re-record list: conformance and golden vectors (D-R7, owned by **TE24**)

| # | Object | Action | Basis |
|---|------|------|------|
| 4.1 | `scripts/mcp-parity/conformance-baseline.yml` | Re-generate by re-running the official referee against the target snapshot (covering the new namespace/header/bearer surfaces); this file §2's commands + the archive path of `run-parity-suite.sh conformance` | R7.2.4.4, design §4.9 |
| 4.2 | `tests/golden_config_merge.rs` / `golden_config_hash.rs` / `golden_glob.rs` / `golden_search.rs` / `golden_tsshape.rs` | Re-record against the v2.32.1 pure-function semantics; register every G2 expectation change case by case | design §4.9 |
| 4.3 | `scripts/mcp-parity/render-call-fixtures.json` + the `render-call-parity` outputs | Re-record the rendering surface against the new tag (tool names/descriptions/render branches) | R7.2.10 |

## 5. New behavioral-parity surfaces (D-R7b, owned by TE21/TE22/TE24)

| Surface | Owner | Acceptance form |
|----|------|----------|
| Approval argument scoping (argument B still intercepted after argument A was approved) | TE21 | fixture cases + persistence across `/resume` restore |
| Backoff visibility (status/diagnostics after consecutive failures) | TE22 | **Landed** (`tests/te22_backoff_oauth.rs`: failure injection → per-surface assertions over status/list/search/describe/instructions/direct + expiry recovery + `/mcp status\|tools`) |
| 503/202/401 classification | TE22 | protocol-leg response-classification vectors (the 401 leg `http-auth-401` MATCHes on both the regression and target tracks; 503/202 belong to TE24) |
| Nested parameters (stable hashing for object/array arguments) | TE21 | argument-scoping vectors |
| OAuth 401 and re-registration | TE22 | **Landed** (`tests/te22_backoff_oauth.rs`: MemorySecretStore injection + 401 compare-and-delete; stub AS `invalid_grant` → DCR request body asserted field by field); `run-oauth-parity.mjs` MATCHes on both the regression and target tracks |

## 6. Shapes/policies

- The target track reuses the existing harness's normalization allowlist and exemptions (see `README.md`); new surfaces register their
  exemptions and rationale in the corresponding task
  documents.
- Re-recorded outputs are committed to git as evidence chains (consistent with the existing `fixtures/generated/mcp-parity/`), with normalization
  stripping run-time-volatile values.
- Division of labor between this file and `scripts/subagents-parity/expected-target-diffs.json`: the mcp-side re-records were done in one
  pass by TE23/TE24 (goldens replaced directly); the subagents side, implemented in batches, transitioned through the target track's attribution
  list.
