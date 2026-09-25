//! V15-13 FR-Bench (rpi#53): render-efficiency benchmark harness.
//!
//! Reproduces the issue #53 measurement scenarios so the O(lines²) fix
//! cannot silently regress. These are `#[ignore]`d benchmarks, not
//! assertions on wall time (CI machines vary); run with:
//!
//! ```text
//! cargo test -p rpi-tui --test render_bench --release -- --ignored --nocapture
//! ```
//!
//! Scenarios (issue #53, 100-col width, CJK content):
//! - `fresh_render_lines_scaling`: fresh `Markdown::render` (per-delta
//!   cache-miss shape) on 3500 short lines vs 35 long lines of the same
//!   volume — the quadratic factor must track source line count, and after
//!   V15-13 L1 the short-line case must be in the same ballpark as the
//!   long-line case (linear in lines).
//! - `fresh_render_size_scaling`: 10/50/100/200 KB accumulated CJK
//!   documents — per-size fresh render for the folded-delta cost table.

use std::sync::Arc;
use std::time::Instant;

use rpi_tui::components::markdown::{Markdown, MarkdownTheme};
use rpi_tui::tui::Component;

/// CJK line filler ("行{i} " + CJK padding) — worst-case short-line thinking
/// content per issue #53.
fn cjk_doc(lines: usize, chars_per_line: usize) -> String {
    let mut text = String::new();
    for i in 0..lines {
        let mut line = format!("行{i} ");
        while line.chars().count() < chars_per_line {
            line.push('中');
        }
        text.push_str(&line);
        text.push('\n');
    }
    text
}

fn fresh_render(text: &str) -> (std::time::Duration, usize) {
    let markdown = Markdown::new(
        text.to_string(),
        1,
        0,
        Arc::new(MarkdownTheme::identity()),
        None,
        None,
    );
    let start = Instant::now();
    let lines = markdown.render(100);
    (start.elapsed(), lines.len())
}

#[test]
#[ignore = "V15-13 FR-Bench: run explicitly with --release --ignored --nocapture"]
fn fresh_render_lines_scaling() {
    // Issue #53 variable isolation: 3500 short lines (~150 KB) vs 35 long
    // lines (~5.1 MB, same identity theme). Before L1: ~127 ms vs ~200 ms
    // (line-count quadratic). After L1 both must be line-linear.
    let (short, short_out) = {
        let doc = cjk_doc(3500, 12);
        let (elapsed, out) = fresh_render(&doc);
        println!(
            "fresh Markdown::render 3500 short lines (~{} KB): {:?} ({} out)",
            doc.len() / 1024,
            elapsed,
            out
        );
        (elapsed, out)
    };
    let (long, long_out) = {
        // ~5.1 MB total (issue #53: 35 lines x ~146 KB) — CJK chars are
        // 3 bytes, so ~48_600 chars/line.
        let doc = cjk_doc(35, 48_600);
        let (elapsed, out) = fresh_render(&doc);
        println!(
            "fresh Markdown::render 35 long lines (~{}.{} MB): {:?} ({} out)",
            doc.len() / 1024 / 1024,
            (doc.len() / 1024 % 1024) * 10 / 1024,
            elapsed,
            out
        );
        (elapsed, out)
    };
    // Line-linear sanity: the 3500-line case must not be slower than the
    // 5.1 MB case (it renders ~3% of the bytes). Soft assertion — the
    // printed table above is the recorded evidence.
    assert!(
        short <= long,
        "short-line case ({short:?}) must not exceed the 5.1 MB case ({long:?}); \
         {short_out}/{long_out} output lines"
    );
}

#[test]
#[ignore = "V15-13 FR-Bench: run explicitly with --release --ignored --nocapture"]
fn fresh_render_size_scaling() {
    // Issue #53 table: 10/50/100/200 KB accumulated CJK thinking. Before
    // the fix the per-size cost was ~3.8/18.5/62/227 ms (clean 4x per 2x).
    for kb in [10usize, 50, 100, 200] {
        let doc = cjk_doc(kb * 1024 / 42, 12); // ~42 bytes per short line
        let (elapsed, out) = fresh_render(&doc);
        println!(
            "fresh Markdown::render ~{kb} KB ({} lines): {:?} ({} out)",
            doc.lines().count(),
            elapsed,
            out
        );
    }
}
