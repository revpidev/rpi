# Review: `crates/pir-tui/src/utils.rs` + `src/terminal_colors.rs` (port of external/pi @ 2efa728d)

Review type: read-only. No files modified. `plan.md`/`progress.md` do not exist at the repo
root (task-mentioned paths are absent); review proceeded from the code and the pinned
upstream submodule (`external/pi` HEAD = `2efa728d2ee90ef597626e96b1e28ef2b279f07c` = the
pin in UPSTREAM.md — verified via `scripts/verify-upstream.sh` logic).

## Verification performed (evidence)

1. **Exhaustive `visible_width` cross-check against the actual upstream runtime**
   (Node v24.19.0, `external/pi/packages/tui/src/utils.ts`):
   - full single-codepoint sweep (1,112,064 codepoints, surrogates excluded),
   - 400,000 random multi-codepoint graphemes from a curated pool (CJK, marks,
     Thai/Lao AM, RI flags, tag sequences, ZWJ sequences, keycaps, VS16, ANSI/OSC-8),
   - 20,000 random long ANSI/tab strings.
   **Result: 1,532,064 cases, 0 mismatches.**
2. **Function-level cross-check** (`truncate_to_width`, `wrap_text_with_ansi`,
   `slice_by_column`/`slice_with_width`, `extract_segments`, `normalize_terminal_output`,
   `apply_background_to_line`, `is_whitespace_char`, `is_punctuation_char`):
   - 19,530 structured vectors (texts × widths × ellipses × pad × strict),
   - 31,020 edge vectors (swallowed CSI `\x1b[?25l\x1b[31m`, OSC-8 with/without params,
     unterminated CSI/OSC, lone ESC/BEL, tabs-only, width 0, 1000-char lines),
   - 888 extra wrap/truncate vectors (empty leading/trailing lines, CR/CRLF,
     width-1 CJK overflow, whitespace-only lines).
   **Result: 51,438 vectors, 0 real mismatches** (only 6 false positives caused by my
   harness splitting JS strings into UTF-16 units vs Rust chars; all values identical).
3. **Generated tables**: all 11 Unicode tables verified sorted and range-merged
   (required for the `binary_search`-based `in_ranges`/`contains_sequence`); the 3 tag
   sequences and 1,245 ZWJ sequences are lexicographically sorted. `RGI_FLAG_PAIRS`
   (259) sorted.
4. **Tests**: `cargo test -p pir-tui --lib utils` → 38/38 pass; `terminal_colors` → 9/9
   pass; `cargo clippy -p pir-tui --lib` clean.
5. **Performance sanity** (release build): `visible_width` on a 200KB mixed
   CJK/emoji/ANSI line ≈ 16 ms; `wrap_text_with_ansi` at width 80 ≈ 27 ms (5,000 lines);
   600KB ASCII fast path ≈ 119 µs. Linear, no quadratic blow-up (the inner ANSI scans
   are O(1) per position for non-ESC bytes; every position is visited ~2× worst case).

**Conclusion: the port is behaviorally equivalent to upstream on every input tested
(~1.58M vectors). No high-severity findings.**

---

## Findings (by severity)

### 中 (medium)

**M1 — terminal_colors.rs:209-220 (`parse_terminal_color_scheme_report`): accepts
trailing uppercase `N`; upstream regex has no `i` flag.**
- Problem: the port is case-insensitive (`eq_ignore_ascii_case`), so
  `"\x1b[?997;1N"` → `Some(Dark)` / `"\x1b[?997;2N"` → `Some(Light)`. The pinned
  upstream regex is `/^\x1b\[\?997;(1|2)n$/` (terminal-colors.ts:69) — **no `i` flag**
  (verified by direct node run: `1N`/`2N` → `undefined`). The Rust doc comment
  ("The upstream regex carries the `i` flag, so a trailing `N` is accepted as well",
  line 209) is factually wrong, and the test
  `test_parse_terminal_color_scheme_report_accepts_uppercase_n` (lines 225-231) codifies
  the divergence with the same false justification.
- Practical impact: negligible (terminals emit lowercase `n`), but it is a real
  behavioral deviation from the pinned upstream and the documentation misstates the
  upstream source.
- Suggestion: either drop the case-insensitivity and the test (matching 2efa728d), or —
  if case-insensitivity is intended as a robustness improvement — fix the comment/test
  to describe it as an intentional difference instead of claiming the upstream regex has
  the `i` flag.

### 低 (low)

**L1 — terminal_colors.rs:93 (`parse_osc11_background_color`, `.trim()`): U+0085 (NEL)
trimming diverges from JS `trim()`.**
- Problem: Rust `str::trim()` uses `char::is_whitespace`, which includes U+0085 (NEL,
  White_Space=Yes). JS `trim()` uses the ECMA-262 set, which excludes U+0085.
  `"\x1b]11;\u0085#ffffff\u0085\x07"` → Rust `Some(255,255,255)`, upstream `undefined`
  (verified). Any other \s difference (U+180E, U+200B, U+FEFF) was checked and matches.
- Practical impact: negligible — terminals do not wrap OSC 11 responses in NEL.
- Suggestion: if byte-fidelity is the goal, use a JS-`trim`-equivalent helper
  (the module already has `is_js_whitespace`/`trim_end_js` in utils.rs that implement
  the ECMA-262 set); note `trim_end_js` strips from the end only, so a leading
  trim helper would be needed.

**L2 — terminal_colors.rs:31-42 (`parse_osc_hex_channel`): channels longer than 16 hex
digits overflow `u64::from_str_radix` → `None`; upstream `parseInt` (float) accepts any
length.**
- Problem: `rgb:`/`rgba:`/12-digit-hex channels with >16 hex digits (e.g.
  `rgb:ffffffffffffffff0/8000/ffff`) parse upstream (`{255,128,255}`, verified) but
  return `None` in Rust (verified).
- Practical impact: negligible — real channels are 1-4 hex digits.
- Suggestion: use `u128`/checked arithmetic or match upstream by falling back to
  float math, if absolute fidelity is desired; otherwise document the cap.

**L3 — utils.rs:405-418 (`WidthCache::insert`): duplicate queue entries under
concurrent cache misses can skew eviction.**
- Problem: `visible_width` only calls `insert` on a miss, but two threads missing on the
  same key both insert it: the map dedupes but `order` gets two entries. When the first
  duplicate reaches the front it evicts the still-live map entry (the second queue entry
  then no-ops), so a key can be evicted "early". Bounded (queue ≤ map+1 ≈ 512) and, as
  the header notes, eviction policy is unobservable — hence low severity.
- Suggestion: skip the push if the key already exists in the map (a `HashMap::insert`
  return check), which makes the queue exactly mirror the map.

### 提示 (informational)

**N1 — utils.rs:105-120 (`is_rgi_emoji`, `[base, 0xfe0f]` arm): classification differs
from upstream `\p{RGI_Emoji}` for `base+VS16` where the base already has
Emoji_Presentation=Yes.**
- `EMOJI_BASIC_BASES` (from the Basic_Emoji section of emoji-sequences.txt) includes
  Emoji_Presentation=Yes bases (e.g. U+231A, U+2B50, U+1F600), so Rust treats
  `"⭐️"`/`"⌚️"`/`"😀️"` as RGI while the runtime's `rgiEmojiRegex` returns false
  (verified in node). **Unobservable**: for every such base the width is 2 either way —
  the exhaustive sweep + curated corpus confirm zero width differences (all
  Emoji_Presentation=Yes bases have EAW=2 in npm's data, and RIs short-circuit earlier
  at the RI check). Only affects the private `is_rgi_emoji` result itself.

**N2 — utils.rs:429-483 (`visible_width`): per-miss cost is a bit heavier than
upstream.**
- On each cache miss the input is copied 2-3× (`str.to_string()` key, `clean` clone,
  tab-replace, ANSI-strip allocs) and the global `Mutex` is acquired twice (get + set).
  Upstream JS shares strings by reference. Measured impact is small (16 ms for a 200KB
  mixed line) and the 512-entry cache absorbs repeats; still, if the TUI later renders
  very large frames, consider keying the cache by hash or returning the guard once
  (`let mut cache = width_cache(); ... cache.get_or_insert...` pattern instead of two
  separate acquisitions) to halve lock traffic.

**N3 — utils.rs:450, 508: dead `unwrap_or('\u{fffd}')` fallbacks.**
- `clean[i..].chars().next()` / `normalized[i..].chars().next()` on valid UTF-8 is
  always `Some`; the fallback is unreachable. Harmless; could be `.unwrap()` or a
  `debug_assert`. (Also means a lone `\u{FFFD}` can never be synthesized by these loops.)

**N4 — utils.rs:533-553 (`extract_ansi_code`): CSI final bytes limited to
`m|G|K|H|J`, OSC/APC terminators only BEL/ST — byte-exact with upstream.**
- Sequences such as `\x1b[?25l` or `\x1b[2A` are not recognized (both ports return
  null), so the ESC contributes 0 but `[?25l` counts ~5 visible columns in
  `visible_width`. This is a faithful reproduction of an upstream quirk — confirmed by
  the cross-check corpus (all matched). Only relevant if later "improved" — don't
  diverge without an intentional-difference note.

**N5 — utils.rs:84-89: `could_be_emoji` doc comment mentions "narrowing them would
change widths of e.g. U+25FD/U+25FE", but U+25FD/U+25FE are not inside any of the four
codepoint ranges (they are 0x25xx; ranges are 1F000-1FBFF, 2300-23FF, 2600-27BF,
2B50-2B55) — the comment's example appears off, though the ranges themselves are
byte-exact with upstream and behavior matches. Comment-only nit.

**N6 — Coverage/residual risk:** the single-codepoint sweep is exhaustive, but the
multi-codepoint space is infinite; the 400k random graphemes + curated sequences give
high confidence, yet a wrong entry among the 1,245 ZWJ sequences would only be caught
when that exact sequence is rendered. Also, `unicode-segmentation` 1.13.3 (Unicode 17
data) vs ICU 76 grapheme rules are claimed verified on a "cross-check corpus" (see
module header) — no mismatch was detected by my random corpus, but that claim lives in
the header, not in a checked-in test.

---

## Summary

- **Correct**: width computation (EAW correction set, zero-width/DI/Mark tables, RGI
  emoji), ANSI parse/wrap/truncate/slice/extract, tab/OSC-8 handling, whitespace
  semantics — all verified equivalent to upstream 2efa728d on ~1.58M vectors including
  an exhaustive codepoint sweep; tests (47) pass; clippy clean; tables sorted.
- **Fixed**: none (review-only).
- **Blocker**: none.
- **Note**: the only true behavioral deviations are in `terminal_colors.rs` (M1, L1,
  L2) — all three on inputs that do not occur in practice, but M1 is enshrined by a
  test with a false claim about the upstream regex and should be corrected or
  re-labeled as an intentional difference.