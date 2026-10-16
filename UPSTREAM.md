# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `1.1.0` (coding-agent; released tag, no unreleased commits included) |
| Git commit | `abe508e1b89912adde45528136c3221eb69acdd7` |
| Short hash | `abe508e1b` |
| Commit message | `Release v1.1.0` |
| Commit date | 2026-10-07 |

Plugin reference pins (move only via the same ADR process; verified by `scripts/verify-upstream.sh`):

| Submodule | Git commit | Upstream tag |
|------|-------|------|
| `external/pi-subagents` | `7d072b91dca7e1aa915282f0159cec0175d65efa` | v0.76.1 |
| `external/pi-mcp-adapter` | `5884ac4e45f5834f51ab8914f61b6a36a6a0a51b` | v4.0.0 |
| `external/rpiv-mono` | `afc21bc25f7d07cae9fb3103dc309240a16b9f49` | v2.13.8 |
| `external/agent-smart-fetch` | `b01116124971de44f16a4477e34c06ba2ab1d0bf` | v0.3.17 |

Upgrade note: v0.1.7 re-baselined (ADR-0036, 2026-10-10) from `a13d35a74` (v1.0.0, ADR-0035) to `abe508e1b` (**v1.1.0 tag itself**), spanning 139 commits / 434 files (+24807/−6953) across five release cycles (v1.0.1 / v1.0.2 / v1.0.3 / v1.0.4 / v1.1.0). The host-side deltas include three [BREAKING] surfaces — the Azure provider rename `azure-openai-responses` → `azure` (#9714; rpi diagnoses the old keys and prints a migration guide instead of migrating user config — 2026-10-10 decision), `--tools` keeping MCP tools by default (v1.0.4), and Home/End moving to `Ctrl+Home`/`Ctrl+End` in fullscreen (#10314) — plus project-level MCP server overrides (`.pi/mcp.json` entries without `command`/`url` toggle enabled/exposure only, #10277), MCP OAuth CIMD (`oauth.clientRegistration: "cimd"`, #10302) and the native-client / close-race / cancel family (#10493/#10249/#10565), `samplingParamsByThinkingLevel` (#9776), `registerToolRenderer()` (#10285), the codemode increments (`image()` temp-file writes, `tools.read()` image blocks, prelude freeze #10444, output caps #10283), `prepareLoadout` hidden-tool prompt isolation (`ToolLoadout.getPromptGuidelines()`, #10343), OSC 7501 program-status reporting (`PI_PROGRAM_STATUS`, #10607), the live `/mcp` manager (#10562), Claude Haiku 5.5, the GPT-6 Luna image-accepting classifier, llama.cpp native decision models (`/v1/systemone`, #10382), additive extension surfaces (render context `durationMs`/`outputPad`, `agent_settled.aborted`), and the managed-install/CLI/TUI fix family. The session file format and the embedded export-html viewer assets are unchanged in the range (V16-02/V16-03 landed work stays valid). The pi-durable accumulated surface is ported directly this cycle under the [PARITY] decision with the upstream self-declared experimental semantics carried over verbatim (ADR-0036 decision 3), and pi-mcp-adapter is removed from the pin set as the plugin retires this cycle (ADR-0037 — its v4.0.0 row above stays until TE49 de-lists it). Plugin reference pins: pi-subagents moves to v0.76.1 (ADR-0036 decision 2); rpiv-mono and agent-smart-fetch are unchanged. **Annex (2026-10-16, TE48):** the FR-F startup recheck found rpiv-mono had released v2.13.0–v2.13.8 with behavior changes in both ported packages (`rpiv-ask-user-question` inline presentation/RPC/notes, `rpiv-todo` codemode snapshot replay); the coordinator directed TE48 to follow the pin, so `external/rpiv-mono` now pins **`afc21bc2` (v2.13.8)** and both first-party plugins were rebased in the same change set (agent-smart-fetch stays zero-diff at `b0111612`). The post-tag main overflow — pi +14 commits (two of them `feat(durable)`), pi-subagents +56 (2026-10-10 fetch snapshot) — and every 1.1.1+ release are deferred to v0.1.8 (ADR-0036 decisions 4/7). ADR-0035's non-pin decisions carry forward unchanged (codemode/MCP full-alignment built-in route; ADR-0001's no-JS-engine red line as amended to permit only the QuickJS-via-WASM (wasmtime) sandbox form; TE-D44 disposition). The extension-visible additive surfaces fold into this version's single ABI minor bump (ADR-0013). The change requirements and design live in the separate documentation repository (not public).

Historical baselines: v0.1.7 pinned `abe508e1b` (v1.1.0, 2026-10-07, ADR-0036); v0.1.6 initially pinned `f07218c4d` (v0.87.1, 2026-09-22, ADR-0032), re-baselined mid-cycle to `005af57d8` (v0.99.2, 2026-09-30, ADR-0034) and again to `a13d35a74` (v1.0.0, 2026-10-01, ADR-0035); v0.1.5 pinned `19451accd` (v0.86.1+1, 2026-09-20, ADR-0031; initial pin `d1230ea` v0.86.0+2, ADR-0029, re-pinned in-cycle the same day); v0.1.4 pinned `9841914c` (v0.85.0+, 2026-09-05, ADR-0023); v0.11 pinned `4181f66e` (v0.84.1+, 2026-08-08, ADR-0012); v0.1 pinned `2efa728d` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: abe508e1b89912adde45528136c3221eb69acdd7
```
