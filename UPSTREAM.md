# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `0.85.0` (coding-agent; HEAD includes 1 unpublished fix after 0.85.0) |
| Git commit | `9841914c71a74d81abe07f751aefd271fd924e63` |
| Short hash | `9841914` |
| Commit message | `fix(tui): keep list selection unchanged on mouse hover` |
| Commit date | 2026-09-05 |

Upgrade note: v0.1.4 raised the comparison baseline from `4181f66` (v0.84.1+) to `9841914` (v0.85.0+), spanning 698 commits / 866 files. The change requirements and design live in the separate documentation repository (not public); the pin upgrade decision is ADR-0023.

Historical baselines: v0.11 pinned `4181f66e6b3ccbef760c2966ecd8b596b926fec6` (v0.84.1+, 2026-08-08); v0.1 pinned `2efa728d2ee90ef597626e96b1e28ef2b279f07c` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: 9841914c71a74d81abe07f751aefd271fd924e63
```
