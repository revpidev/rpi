# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `0.87.1` (coding-agent; released tag, no unreleased commits included) |
| Git commit | `f07218c4d4bbc12bef056a7058c3dd49dfe41abe` |
| Short hash | `f07218c4d` |
| Commit message | `Release v0.87.1` |
| Commit date | 2026-09-22 |

Upgrade note: v0.1.6 raised the comparison baseline from `19451accd` (v0.86.1+1) to `f07218c4d` (**v0.87.1 tag itself** — unlike the v0.1.5 HEAD+N precedent, the 77 unreleased main commits after the tag are excluded: the codemode+MCP built-in-extensions rework (#10040) plus virtual models, Kimi K3 swap and durable progress are deferred to v0.1.7 pending a dedicated ADR), spanning 36 commits / 219 files (the v0.87.0 and v0.87.1 release cycles; behavior surface: canonical session context and context edits with the agent-boundary BREAKING cluster, `context_with_system` extension event, per-model image input limits, non-strict default for unknown endpoints, the Opus 5.5 / GPT-6 Sol & Luna / Grok 4.7 model family, `--mode` validation). The pin upgrade decision is ADR-0032, which also moves the four plugin reference repos to their latest released tags in one step (pi-subagents v0.73.1 `8a403efb` · pi-mcp-adapter v3.2.0 `b6e06a16`, v3.0 BREAKING config migration · rpiv-mono v2.11.0 `61904e6`, plugin packages zero-behavior · agent-smart-fetch unchanged at HEAD). The change requirements and design live in the separate documentation repository (not public).

Historical baselines: v0.1.5 pinned `19451accd` (v0.86.1+1, 2026-09-20, ADR-0031; initial pin `d1230ea` v0.86.0+2, ADR-0029, re-pinned in-cycle the same day); v0.1.4 pinned `9841914c` (v0.85.0+, 2026-09-05, ADR-0023); v0.11 pinned `4181f66e` (v0.84.1+, 2026-08-08, ADR-0012); v0.1 pinned `2efa728d` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: f07218c4d4bbc12bef056a7058c3dd49dfe41abe
```
