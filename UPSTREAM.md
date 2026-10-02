# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `1.0.0` (coding-agent; released tag, no unreleased commits included) |
| Git commit | `a13d35a742c6ef8462812a28fbe1d8c8b7431c32` |
| Short hash | `a13d35a74` |
| Commit message | `Release v1.0.0` |
| Commit date | 2026-10-01 |

Plugin reference pins (move only via the same ADR process; verified by `scripts/verify-upstream.sh`):

| Submodule | Git commit | Upstream tag |
|------|-------|------|
| `external/pi-subagents` | `b6bda32f03b7f549623bc404c9be14dca298ddc4` | v0.74.0 |
| `external/pi-mcp-adapter` | `5884ac4e45f5834f51ab8914f61b6a36a6a0a51b` | v4.0.0 |
| `external/rpiv-mono` | `7c9bc924c5bfd148f36d7ebc9f7bd0a9469d633f` | v2.12.0 |
| `external/agent-smart-fetch` | `b01116124971de44f16a4477e34c06ba2ab1d0bf` | v0.3.17 |

Upgrade note: v0.1.6 re-baselined a second time mid-cycle (ADR-0035, 2026-10-02) from `005af57d8` (v0.99.2, ADR-0034) to `a13d35a74` (**v1.0.0 tag itself** — fullscreen-by-default TUI, leaner codemode prompt (~40% fewer tokens) with script error recovery (`guard()` proxies; `typeof tools.x` probing replaced by `"x" in tools`), `models.generateImages()` in codemode, MCP OAuth hardening (`oauth.authServerMetadataUrl`, RFC 9207 `iss` checks, per-server-name+URL credential storage with in-place migration, step-up scope keeping), deferred-MCP-tool restore on resume/`/reload`, Anthropic copy-code login, Radius sign-in in `/login` with one-step MCP setup, `--provider`-without-`--model` fail-fast, TUI fix family (transcript memory, ANSI slice boundaries, selection color bleed, slash completion after whitespace, pastel chroma in system theme), pi-agent-core experimental harness removal [BREAKING, no rpi consumer surface], and the pi-durable 1.0.0 initial release [still self-declared experimental; DEFER maintained]), spanning 46 commits / 601 files from the `005af57d8` pin. The session file format and the extension event surface are unchanged in the range (V16-02/V16-03 landed work stays valid; the only on-disk format change is the `mcp-auth.json` credential key migration). Plugin reference pins are unchanged (ADR-0035 decision 2; the four repos' post-tag main overflow — subagents +23 / mcp-adapter +38 / rpiv-mono +4 / smart-fetch 0 (2026-10-02 06:57 fetch snapshot) — is deferred to v0.1.7 intake). ADR-0034's non-pin decisions carry forward unchanged (codemode/MCP full-alignment built-in route; ADR-0001's no-JS-engine red line as amended to permit only the QuickJS-via-WASM (wasmtime) sandbox form; TE-D44 disposition). The change requirements and design live in the separate documentation repository (not public).

Historical baselines: v0.1.6 initially pinned `f07218c4d` (v0.87.1, 2026-09-22, ADR-0032), re-baselined mid-cycle to `005af57d8` (v0.99.2, 2026-09-30, ADR-0034) and again to `a13d35a74` (v1.0.0, 2026-10-01, ADR-0035); v0.1.5 pinned `19451accd` (v0.86.1+1, 2026-09-20, ADR-0031; initial pin `d1230ea` v0.86.0+2, ADR-0029, re-pinned in-cycle the same day); v0.1.4 pinned `9841914c` (v0.85.0+, 2026-09-05, ADR-0023); v0.11 pinned `4181f66e` (v0.84.1+, 2026-08-08, ADR-0012); v0.1 pinned `2efa728d` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: a13d35a742c6ef8462812a28fbe1d8c8b7431c32
```
