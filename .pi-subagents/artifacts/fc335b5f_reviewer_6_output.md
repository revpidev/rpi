# Review: `crates/pir-tui/src/terminal_image.rs` (Rust port of upstream `packages/tui/src/terminal-image.ts` @ 2efa728d)

## Scope & method

- Upstream verified: `external/pi` submodule HEAD = `2efa728d`; `terminal-image.ts` last touched by `7a14325b` (Warp detection), included in 2efa728d. The file reviewed is exactly the commit in question.
- Line-by-line comparison of all 27 exported functions against upstream.
- **Differential execution**: ran the *actual* upstream `terminal-image.ts` under Node 24 type-stripping (`/tmp/upstream-driver.ts`) with ~50 cases (kitty chunking at 4096/4097/3×4096+100 boundaries, iTerm2 params, cell-size math, all 4 dimension parsers, 18-env-var capability matrix, renderImage both protocols, imageFallback, hyperlink, isImageLine, allocateImageId range), and compared against the Rust port's own test assertions and code.
- `cargo test -p pir-tui terminal_image`: **74 passed, 0 failed**; `cargo clippy -p pir-tui`: clean for this file.

## Verdict

The port is faithful. **No high or medium findings.** Every differential case matched; the capability-detection matrix (including branch *order*: tmux → screen → kitty → ghostty → wezterm → warp → iterm2 → WT → vscode → alacritty → jetbrains → unknown) is identical to upstream, including subtle details such as `term.includes("ghostty")`, Warp's three detection paths, `jetbrains-jediterm` hyperlinks=false, and conservative unknown-terminal defaults. Kitty chunking (`m=1`/`m=0`, 4096, params-on-first-chunk-only), `C=1` cursor suppression, `i=`/`d=I`/`d=A` delete commands, strict-vs-lenient base64 (disclosed in module docs), and f64 mirroring of `Math.floor`/`Math.ceil`/`Math.min`/`Math.max` all check out.

Findings below are 低/提示 level only.

---

## 低 (Low)

### L1 — `image_fallback` diverges from upstream for an empty filename
- **Location:** `crates/pir-tui/src/terminal_image.rs:724-734` vs upstream `terminal-image.ts:482-487`
- **Problem:** Upstream `if (filename) parts.push(filename)` treats `""` as falsy and omits it, producing `[Image: [image/png]]`. The Rust port uses `if let Some(filename) = filename { parts.push(...) }`, so `filename: Some("")` pushes an empty string and yields `[Image:  [image/png]]` (double space). This is the *only* truthiness mirroring gap in the file: `encode_iterm2` correctly filters empty names (`filter(|n| !n.is_empty())`) and `encode_kitty` correctly omits zeros, but `image_fallback` does not filter.
- **Evidence:** differential run — upstream `imageFallback("image/png", undefined, "")` → `"[Image: [image/png]]"`; Rust code path pushes `""`. Cosmetic-only (empty filename is degenerate input), but it is a real behavioral divergence from upstream and inconsistent with the port's own documented convention.
- **Suggestion:** `if let Some(filename) = filename.filter(|n| !n.is_empty()) { parts.push(filename.to_string()); }`, or mirror upstream truthiness with a comment. Optionally add a test for `Some("")`.

### L2 — `probe_tmux_hyperlinks`: `try_wait` error path returns without reaping the child
- **Location:** `crates/pir-tui/src/terminal_image.rs:126-168` (loop at 136-153)
- **Problem:** On `Err(_)` from `child.try_wait()` (line 151) the function `return false` without `kill()`/`wait()`. In practice `try_wait` errors (e.g., `ECHILD`) are near-unreachable, but if one occurred while the child was still running, the child would be left unreaped (zombie). Upstream's `execSync` has no equivalent path (it always reaps on throw).
- **Suggestion:** On the `Err` arm, attempt `let _ = child.kill(); let _ = child.wait();` before returning `false`.

---

## 提示 (Hint)

### H1 — `probe_tmux_hyperlinks`: timeout is 250–260 ms with SIGKILL vs upstream's exact 250 ms SIGTERM
- **Location:** `crates/pir-tui/src/terminal_image.rs:140-148` vs upstream `terminal-image.ts:51-55`
- **Problem:** The 10 ms poll granularity means a stuck tmux is killed between 250 ms and 260 ms, and Rust `Child::kill` sends SIGKILL while Node's `execSync` timeout sends SIGTERM. Behaviorally equivalent for the stated contract (both fall back to `false`), and the module doc's "same 250 ms budget" claim is accurate within poll granularity. No action required unless exact timing matters.
- **Suggestion:** None required; optionally note the granularity in the doc comment. Also consider `child.kill()` + `wait()` ordering is already correct for reaping.

### H2 — `calculate_image_cell_size`: NaN/saturation behavior differs from JS for degenerate inputs
- **Location:** `crates/pir-tui/src/terminal_image.rs:469-508` vs upstream `terminal-image.ts:257-281`
- **Problem:** For finite inputs the f64 mirroring is exact (verified: 2×2, 1×5, 1×1, 5×3, rows=8 cases all match). Divergences only under degenerate `NaN`/huge inputs: (a) `f64::min`/`f64::max` ignore NaN while JS `Math.min`/`Math.max` propagate it — e.g., `cell_dimensions.width_px = NaN` yields `columns as u32` → `0` (Rust NaN→int cast) where upstream would emit `c=NaN`; (b) `columns as u32` saturates at `u32::MAX` for `max_width_cells ≥ 2^32`, where upstream emits the raw number. Unreachable through the image parsers (all produce finite `u32 as f64`); only via direct API misuse.
- **Suggestion:** None required; optionally guard public entry points with `is_finite()` if hard robustness is desired.

### H3 — `encode_kitty`: invalid (non-ASCII) base64 input silently drops chunks
- **Location:** `crates/pir-tui/src/terminal_image.rs:387-388` vs upstream `terminal-image.ts:191-192`
- **Problem:** `base64_data.get(offset..end).unwrap_or_default()` returns `""` when the byte range isn't a UTF-8 char boundary, so a chunk is silently skipped and the emitted sequence is truncated. Upstream `String.prototype.slice` includes the characters regardless. Input with non-ASCII chars is invalid base64 anyway (both produce garbage), and the comment at 386-387 discloses the total-function tradeoff. Note only.
- **Suggestion:** None required; invalid input is out of contract.

### H4 — `allocate_image_id`: negligible modulo bias vs `Math.random()`
- **Location:** `crates/pir-tui/src/terminal_image.rs:324-337` vs upstream `terminal-image.ts:155-163`
- **Problem:** `hasher.finish() as u32 % 0xffff_fffe` — since `2^32 = 0xfffffffe + 2`, residues 0 and 1 occur one extra time per full u32 cycle (~2/2^32 relative bias; truly negligible). Range [1, 0xfffffffe] verified identical (differential run observed min 155656, max 4294944371 within bounds; Rust test `test_allocate_image_id_returns_ids_in_range` also passes). Informational only.

### H5 — 占位行 (placeholder rows) live outside this file
- **Location:** n/a (this file) — upstream `packages/tui/src/components/image.ts:95-102`; Rust equivalent in `crates/pir-tui/src/tui.rs` (`get_kitty_image_reserved_rows` ~2376, `full_render` ~2440)
- **Problem:** The review focus asks about placeholder-row correctness. That logic is *not* part of `terminal-image.ts` — the upstream `Image` component pads `rows-1` empty lines and, for iTerm2, emits `\x1b[{rowOffset}A` cursor-up before drawing. The Rust crate has no `Image` component port yet; `tui.rs` implements row reservation/expansion/cleanup (out of scope here) that relies on this file's `render_image().rows` and `encode_kitty`'s `C=1`/`r=`/`c=` — all present, matching, and tested (`test_render_image_can_opt_into_no_terminal_side_cursor_movement` asserts `,C=1,`; `test_render_image_honors_max_height_cells_by_reducing_rendered_width` asserts `c=1,r=5`). When the Image component is ported, mirror `image.ts`'s placeholder padding and the iTerm2 `\x1b[..A` + sequence + `\x1b[..B` dance.
- **Suggestion:** Track as a follow-up for the Image component port (upstream `image.ts`), not this file.

### H6 — No test exercises the real `probe_tmux_hyperlinks` spawn path
- **Location:** `crates/pir-tui/src/terminal_image.rs:126-168`; tests use injected closures (`detect_capabilities_with(|| true/false)`)
- **Problem:** The actual `tmux` spawn → poll → kill → parse path is validated only by inspection, not by a test (upstream also has no unit test for it). Acceptable; noted for completeness.
- **Suggestion:** Optional: a test that stubs `tmux` via a PATH shim in a temp dir.

---

## Correct (evidence summary)

- **Kitty chunking** — differential: 4096→single chunk (no `m=`), 4097→`m=1`+`m=0`, 3×4096+100→4 chunks with exact lengths [4117, 4103, 4103, 107]; Rust test `test_encode_kitty_chunks_large_payloads` asserts identical values. `C=1` ordering `a=T,f=100,q=2,C=1,c=,r=,i=` matches byte-for-byte.
- **Delete commands** — `\x1b_Ga=d,d=I,i=42,q=2\x1b\\` and `\x1b_Ga=d,d=A,q=2\x1b\\` byte-identical to upstream.
- **ID allocation** — range [1, 0xfffffffe] correct; `% 0xffff_fffe + 1` arithmetic verified.
- **Capability matrix** — all 18 differential env combinations match upstream exactly, including tmux branch precedence, `screen` branch, `term.contains("ghostty")`, Warp env vars, `truecolor`/`24bit` hints, unknown-terminal `hyperlinks:false`.
- **base64/dimensions** — PNG/JPEG/GIF/VP8/VP8L parsers byte-identical logic to upstream on shared fixtures (800×600, 75×120, 320×240, 575×320, 2×9); strict-vs-lenient base64 divergence is disclosed in module docs and intentional.
- **renderImage** — kitty (`C=1,c=2,r=2`), max-height reduction (`c=1,r=5`), iTerm2 (`inline=1;width=2;height=auto`), no-images → None: all match upstream.
- **`env_flag`/`env_var`** — empty-string env vars treated as falsy, mirroring JS truthiness (`process.env.X` is `""` → falsy); `TERM_PROGRAM`/`TERMINAL_EMULATOR` lowercased like upstream.
- **Tests** — 74/74 pass, including exact-sequence assertions; `with_env`/`with_globals` share one state lock so env/global mutations don't race.