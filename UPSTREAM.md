# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `0.99.2` (coding-agent; released tag, no unreleased commits included) |
| Git commit | `005af57d88ee23b33778f343a9595b32e67ff788` |
| Short hash | `005af57d8` |
| Commit message | `Release v0.99.2` |
| Commit date | 2026-09-30 |

Plugin reference pins (move only via the same ADR process; verified by `scripts/verify-upstream.sh`):

| Submodule | Git commit | Upstream tag |
|------|-------|------|
| `external/pi-subagents` | `b6bda32f03b7f549623bc404c9be14dca298ddc4` | v0.74.0 |
| `external/pi-mcp-adapter` | `5884ac4e45f5834f51ab8914f61b6a36a6a0a51b` | v4.0.0 |
| `external/rpiv-mono` | `7c9bc924c5bfd148f36d7ebc9f7bd0a9469d633f` | v2.12.0 |
| `external/agent-smart-fetch` | `b01116124971de44f16a4477e34c06ba2ab1d0bf` | v0.3.17 |

Upgrade note: v0.1.6 re-baselined mid-cycle (ADR-0034, 2026-10-01) from `f07218c4d` (v0.87.1, ADR-0032) to `005af57d8` (**v0.99.2 tag itself** — the previously deferred 77 unreleased commits (codemode+MCP built-in extensions #10040, virtual models #10035, Kimi K3 swap) were released as v0.99.0/v0.99.1/v0.99.2 and are now in scope; v0.99.2 is the effective behavior baseline for codemode/MCP, superseding the 0.99.0/0.99.1 interaction model), spanning 170 commits / 796 files from the v0.1.5 pin `19451accd` (the v0.87.0, v0.87.1, v0.99.0, v0.99.1 and v0.99.2 release cycles; behavior surface: canonical session context and context edits with the agent-boundary BREAKING cluster, `context_with_system`, per-model image input limits, codemode + tool_search + built-in MCP as built-in extensions with the tool-orchestration API (exposure / `prepareLoadout` / `ctx.executeTool`), model catalog schema v6 (chat/image/classifier), system theme by default, virtual models, Sign in with ChatGPT, RPC dispositions). codemode/MCP follow the full-alignment built-in route (ADR-0034 decision 2); `rpi-ext-mcp-adapter` becomes an optional replacement of the built-in MCP. ADR-0034 also moves the plugin reference repos in one step (pi-subagents v0.74.0 `b6bda32f` · pi-mcp-adapter v4.0.0 `5884ac4e` (v4.0 BREAKING: mcpScript opt-in, `builtin:mcp` coexistence) · rpiv-mono v2.12.0 `7c9bc924` · agent-smart-fetch unchanged at HEAD) and amends ADR-0001's no-JS-engine red line to permit the QuickJS-via-WASM (wasmtime) sandbox form only. The change requirements and design live in the separate documentation repository (not public).

Historical baselines: v0.1.6 initially pinned `f07218c4d` (v0.87.1, 2026-09-22, ADR-0032; re-baselined mid-cycle by ADR-0034); v0.1.5 pinned `19451accd` (v0.86.1+1, 2026-09-20, ADR-0031; initial pin `d1230ea` v0.86.0+2, ADR-0029, re-pinned in-cycle the same day); v0.1.4 pinned `9841914c` (v0.85.0+, 2026-09-05, ADR-0023); v0.11 pinned `4181f66e` (v0.84.1+, 2026-08-08, ADR-0012); v0.1 pinned `2efa728d` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: 005af57d88ee23b33778f343a9595b32e67ff788
```
