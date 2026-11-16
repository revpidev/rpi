# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `1.5.2` (coding-agent; released tag, no unreleased commits included) |
| Git commit | `b4afdddf44e059c67ccf5097e5317d913521b470` |
| Short hash | `b4afdddf` |
| Commit message | `Release v1.5.2` |
| Commit date | 2026-11-10 |

Plugin reference pins (move only via the same ADR process; verified by `scripts/verify-upstream.sh`):

| Submodule | Git commit | Upstream tag |
|------|-------|------|
| `external/pi-subagents` | `3a63e0ad8898cf116ce671feabb0f8a7b31731a4` | v0.83.0 |
| `external/rpiv-mono` | `ce6d3530410812ca504ec294995581a52170f9e1` | v2.15.0 |
| `external/agent-smart-fetch` | `b01116124971de44f16a4477e34c06ba2ab1d0bf` | v0.3.17 |

Upgrade note: v0.1.9 re-baselined (ADR-0048, including the V19-00 startup recheck) from `4a080252f` (v1.4.0, ADR-0038) to `b4afdddf` (**v1.5.2 tag itself**), spanning 703 commits / 3211 files (+483679/−39196) across v1.5.0, v1.5.1 and v1.5.2. The reviewed host scope covers provider/catalog and schema changes; session, extension, tool and terminal behavior; codemode and built-in MCP; durable runtime/storage; and the upstream Markdown/highlight reuse (#10971) and incremental tool-argument parsing (#9265) performance mechanisms. Implementation is split across V19-01–V19-07; this pin change does not claim that those behaviors are already ported. The V19-00 target/regression configuration fixes the v1.5.2 target tree `ed418d79cb44d3690f110e1077c4612db033f2c9` and the v1.4.0 regression tree `56ea45bfd3ef589feaddfaf504cfc08d838d812b`; verify both with `python3 scripts/verify-pi-tracks.py`.

Plugin reference pins remain at their v0.1.8 values during V19-00. ADR-0048's final targets are pi-subagents v0.88.1 `83cf25f86b6a70ab917a7e198d0153bc32309f29` (upgraded from the original v0.88.0 target after explicit user confirmation during the V19-00 startup recheck), agent-smart-fetch v0.3.23 `feb6da001161021df6c2de550f225ed5306a671f`, and unchanged rpiv-mono v2.15.0; only V19-03 may switch the two plugin gitlinks after its implementation, dual-track, specification and changelog gates are ready. Post-tag main overflow measured at V19-00 startup — Pi +262, pi-subagents +14 after v0.88.1, rpiv-mono +12, smart-fetch +0 — is excluded. Any newer stable tag requires another ADR review and user confirmation. ADR-0038 is now a historical pin record; its non-pin decisions continue except where ADR-0048 explicitly changes them. The change requirements, task evidence and design live in the separate documentation repository (not public).

Current baseline: the v0.1.9 development branch pins `b4afdddf` (v1.5.2, 2026-11-10, ADR-0048). Historical baselines: v0.1.8 pinned `4a080252f` (v1.4.0, 2026-10-22, ADR-0038); v0.1.7 pinned `abe508e1b` (v1.1.0, 2026-10-07, ADR-0036); v0.1.6 initially pinned `f07218c4d` (v0.87.1, 2026-09-22, ADR-0032), re-baselined mid-cycle to `005af57d8` (v0.99.2, 2026-09-30, ADR-0034) and again to `a13d35a74` (v1.0.0, 2026-10-01, ADR-0035); v0.1.5 pinned `19451accd` (v0.86.1+1, 2026-09-20, ADR-0031; initial pin `d1230ea` v0.86.0+2, ADR-0029, re-pinned in-cycle the same day); v0.1.4 pinned `9841914c` (v0.85.0+, 2026-09-05, ADR-0023); v0.11 pinned `4181f66e` (v0.84.1+, 2026-08-08, ADR-0012); v0.1 pinned `2efa728d` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: b4afdddf44e059c67ccf5097e5317d913521b470
```
