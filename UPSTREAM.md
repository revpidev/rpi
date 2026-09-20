# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `0.86.0` (coding-agent; HEAD includes 2 unpublished commits after 0.86.0) |
| Git commit | `d1230ea2000d876b479a69b8b061f9d670f262f5` |
| Short hash | `d1230ea` |
| Commit message | `fix(coding-agent): preserve multiline bug descriptions` |
| Commit date | 2026-09-20 |

Upgrade note: v0.1.5 raised the comparison baseline from `9841914` (v0.85.0+) to `d1230ea` (v0.86.0+2), spanning 174 commits / 585 files (the v0.85.1 and v0.86.0 release cycles plus 2 unreleased main commits — a changelog placeholder and the `/bug` multiline-description fix). The change requirements and design live in the separate documentation repository (not public); the pin upgrade decision is ADR-0029.

Historical baselines: v0.1.4 pinned `9841914c71a74d81abe07f751aefd271fd924e63` (v0.85.0+, 2026-09-05); v0.11 pinned `4181f66e6b3ccbef760c2966ecd8b596b926fec6` (v0.84.1+, 2026-08-08); v0.1 pinned `2efa728d2ee90ef597626e96b1e28ef2b279f07c` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: d1230ea2000d876b479a69b8b061f9d670f262f5
```
