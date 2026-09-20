# Upstream Pin

The behavioral gold standard of this repository is fixed to the following Pi version. **Do not** change it without first establishing an ADR.

| Item | Value |
|------|-------|
| Remote | https://github.com/earendil-works/pi.git |
| Local | `external/pi/` |
| npm version | `0.86.1` (coding-agent; HEAD includes 1 unpublished commit after 0.86.1) |
| Git commit | `19451accdeec671c1f4da9eafac8fc270f510ef4` |
| Short hash | `19451accd` |
| Commit message | `Add [Unreleased] section for next cycle` |
| Commit date | 2026-09-20 |

Upgrade note: v0.1.5 raised the comparison baseline from `9841914` (v0.85.0+) to `d1230ea` (v0.86.0+2), spanning 174 commits / 585 files (the v0.85.1 and v0.86.0 release cycles plus 2 unreleased main commits — a changelog placeholder and the `/bug` multiline-description fix). The pin upgrade decision is ADR-0029. On the same day upstream released v0.86.1 (10 behavior/doc commits plus the release bump) followed by one changelog-placeholder commit; ADR-0031 re-pinned the baseline in-cycle to `19451accd` (v0.86.1+1), spanning 11 commits / 65 files from `d1230ea` (the v0.86.1 release cycle plus 1 placeholder commit; behavior surface: Meta provider with Muse OAuth, OSC 52 headless clipboard fallback, z.ai prompt-too-long detection, Cerebras strict-mode exclusion, bug-hint suppression). The change requirements and design live in the separate documentation repository (not public).

Historical baselines: v0.1.5 initially pinned `d1230ea2000d876b479a69b8b061f9d670f262f5` (v0.86.0+2, 2026-09-20; re-pinned the same day via ADR-0031); v0.1.4 pinned `9841914c71a74d81abe07f751aefd271fd924e63` (v0.85.0+, 2026-09-05); v0.11 pinned `4181f66e6b3ccbef760c2966ecd8b596b926fec6` (v0.84.1+, 2026-08-08); v0.1 pinned `2efa728d2ee90ef597626e96b1e28ef2b279f07c` (v0.82.1, 2026-07-27).

Intentional differences established by ADR (outside the gold standard): product endpoint defaults moved to `revpi.dev` (including the Cloudflare Pages deployment in the rpi-pages repository); the override chain and upstream endpoint configurability semantics are unchanged.

Verification:

```bash
cd external/pi && git rev-parse HEAD
# expected: 19451accdeec671c1f4da9eafac8fc270f510ef4
```
