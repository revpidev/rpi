# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `1.4.0` (coding-agent; released tag, no unreleased commits included) |
| Git commit | `4a080252f06caf63a525d7873c5d94d3c23887e0` |
| Short hash | `4a080252f` |
| Commit message | `Release v1.4.0` |
| Commit date | 2026-10-22 |

Plugin reference pins (move only via the same ADR process; verified by `scripts/verify-upstream.sh`):

| Submodule | Git commit | Upstream tag |
|------|-------|------|
| `external/pi-subagents` | `7d072b91dca7e1aa915282f0159cec0175d65efa` | v0.76.1 |
| `external/rpiv-mono` | `afc21bc25f7d07cae9fb3103dc309240a16b9f49` | v2.13.8 |
| `external/agent-smart-fetch` | `b01116124971de44f16a4477e34c06ba2ab1d0bf` | v0.3.17 |

Upgrade note: v0.1.8 re-baselined (ADR-0038, 2026-10-26, including the V18-00 startup recheck) from `abe508e1b` (v1.1.0, ADR-0036) to `4a080252f` (**v1.4.0 tag itself**), spanning 225 commits / 906 files (+73831/−42639) across three release cycles (v1.2.0 / v1.3.0 / v1.4.0). The reviewed host scope covers settings and schemas; extension startup, busy state, dialogs and tool reachability; session restore and compaction; provider/catalog, transport, auth, classifier and client identity changes; codemode and built-in MCP; CLI/TUI/platform fixes; and the durable runtime/storage increments. Kimi custom base URLs must include `/v1`; durable schema v2 and its runtime API changes are explicit BREAKING surfaces. The implementation and parity work is tracked separately in V18-01–V18-10 and TE51–TE53, so this pin change does not claim those tasks are complete. `session-manager.ts` and `packages/agent/src/messages.ts` have no diff in this range, while `core/export-html` has five changed files (+62/−46), including the new CJK vendor asset; V18-00 owns the deterministic six-asset synchronizer, CJK consumption and real export smoke, and V18-04 owns the behavioral review. Plugin reference pins intentionally remain at the v0.1.7 values above during V18-00: ADR-0038 targets pi-subagents v0.83.0 `3a63e0ad` and rpiv-mono v2.15.0 `ce6d3530`, but only TE51 may switch those gitlinks after its target/regression parity, implementation, specifications and change notes are ready; agent-smart-fetch remains unchanged. The mcp-adapter startup/update removal guidance is removed separately under ADR-0038 without restoring the plugin or deleting user files. Post-tag main overflow — Pi +67, pi-subagents +1 documentation-only commit, rpiv-mono +3 — is excluded; any newer stable tag requires another ADR review and user confirmation. ADR-0036's non-pin decisions continue except where ADR-0038 explicitly changes them; ADR-0037 is only partially superseded for that guidance. The change requirements, task evidence and design live in the separate documentation repository (not public).

Current baseline: v0.1.8 pins `4a080252f` (v1.4.0, 2026-10-22, ADR-0038). Historical baselines: v0.1.7 pinned `abe508e1b` (v1.1.0, 2026-10-07, ADR-0036); v0.1.6 initially pinned `f07218c4d` (v0.87.1, 2026-09-22, ADR-0032), re-baselined mid-cycle to `005af57d8` (v0.99.2, 2026-09-30, ADR-0034) and again to `a13d35a74` (v1.0.0, 2026-10-01, ADR-0035); v0.1.5 pinned `19451accd` (v0.86.1+1, 2026-09-20, ADR-0031; initial pin `d1230ea` v0.86.0+2, ADR-0029, re-pinned in-cycle the same day); v0.1.4 pinned `9841914c` (v0.85.0+, 2026-09-05, ADR-0023); v0.11 pinned `4181f66e` (v0.84.1+, 2026-08-08, ADR-0012); v0.1 pinned `2efa728d` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: 4a080252f06caf63a525d7873c5d94d3c23887e0
```
