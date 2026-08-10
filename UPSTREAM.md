# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `0.84.1` (coding-agent; HEAD includes a few unpublished changes after 0.84.1) |
| Git commit | `4181f66e6b3ccbef760c2966ecd8b596b926fec6` |
| Short hash | `4181f66` |
| Commit message | `docs(agent): tighten durable harness design` |
| Commit date | 2026-08-08 |

Upgrade note: v0.11 raised the comparison baseline from `2efa728` (v0.82.1) to `4181f66` (v0.84.1+), spanning 461 commits / 655 files. The change requirements and design live in the separate documentation repository (not public).

Historical baseline: v0.1 pinned `2efa728d2ee90ef597626e96b1e28ef2b279f07c` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: 4181f66e6b3ccbef760c2966ecd8b596b926fec6
```
