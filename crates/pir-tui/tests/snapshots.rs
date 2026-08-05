//! Component render snapshot golden files (T11 gate requirement).
//!
//! Each case renders a base component at typical parameters through the
//! public `Component::render(width)` contract and compares the raw ANSI line
//! output (style sequences verbatim) against a golden file in
//! `tests/snapshots/<name>.snap`. The snapshots are observations of the
//! frozen upstream behavior — when an intentional upstream behavior change
//! alters the output, regenerate the goldens with:
//!
//! ```sh
//! PIR_UPDATE_SNAPSHOTS=1 cargo test -p pir-tui --test snapshots
//! ```
//!
//! and review the diff before committing.

use std::fs;
use std::path::PathBuf;

use pir_tui::components::loader::{Loader, LoaderIndicatorOptions};
use pir_tui::components::r#box::Box as TuiBox;
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::components::truncated_text::TruncatedText;
use pir_tui::tui::{Component, Container, RenderHandle};

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

/// Compare `lines` against `tests/snapshots/<name>.snap`; rewrite the golden
/// when `PIR_UPDATE_SNAPSHOTS=1` is set.
fn assert_snapshot(name: &str, lines: &[String]) {
    let actual = format!("{}\n", lines.join("\n"));
    let path = snapshot_dir().join(format!("{name}.snap"));

    if std::env::var_os("PIR_UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(snapshot_dir())
            .unwrap_or_else(|err| panic!("create snapshot dir: {err}"));
        fs::write(&path, &actual).unwrap_or_else(|err| panic!("write {path:?}: {err}"));
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing/unreadable snapshot {path:?}: {err}; regenerate with PIR_UPDATE_SNAPSHOTS=1"
        )
    });
    assert_eq!(
        actual, expected,
        "snapshot {name} drifted; if intentional, regenerate with PIR_UPDATE_SNAPSHOTS=1 \
         (escapes shown via debug formatting)\nactual:   {actual:?}\nexpected: {expected:?}"
    );
}

fn cyan(text: &str) -> String {
    format!("\x1b[36m{text}\x1b[0m")
}

fn blue_bg(text: &str) -> String {
    format!("\x1b[44m{text}\x1b[49m")
}

// --- Text (upstream components/text.ts) -------------------------------------

#[test]
fn text_plain() {
    let text = Text::new("The quick brown fox", 0, 0, None);
    assert_snapshot("text_plain", &text.render(80));
}

#[test]
fn text_padded() {
    let text = Text::new("padded line", 2, 1, None);
    assert_snapshot("text_padded", &text.render(40));
}

#[test]
fn text_ansi_wrap() {
    // ANSI-styled segment wrapped at a narrow width: style sequences must
    // survive wrapping intact.
    let text = Text::new(
        "\x1b[31mred\x1b[0m plain words that wrap around the narrow width",
        0,
        0,
        None,
    );
    assert_snapshot("text_ansi_wrap", &text.render(20));
}

#[test]
fn text_background() {
    let text = Text::new("on blue", 1, 0, Some(Box::new(blue_bg)));
    assert_snapshot("text_background", &text.render(30));
}

// --- Container (upstream Container in tui.ts) -------------------------------

#[test]
fn container_nested() {
    let mut container = Container::new();
    container.add_child(Box::new(Text::new("first", 0, 0, None)));
    container.add_child(Box::new(Spacer::new(1)));
    container.add_child(Box::new(Text::new("second", 1, 0, None)));
    assert_snapshot("container_nested", &container.render(40));
}

// --- Spacer (upstream components/spacer.ts) ---------------------------------

#[test]
fn spacer_lines() {
    assert_snapshot("spacer_lines", &Spacer::new(2).render(20));
}

// --- TruncatedText (upstream components/truncated-text.ts) ------------------

#[test]
fn truncated_text_overflow() {
    let text = TruncatedText::new("a very long single line that must be truncated", 1, 1);
    assert_snapshot("truncated_text_overflow", &text.render(20));
}

#[test]
fn truncated_text_ansi() {
    let text = TruncatedText::new("\x1b[32mgreen text that is far too long\x1b[0m", 0, 0);
    assert_snapshot("truncated_text_ansi", &text.render(15));
}

// --- Box (upstream components/box.ts) ---------------------------------------

#[test]
fn box_with_background() {
    let mut boxed = TuiBox::new(1, 1, Some(Box::new(blue_bg)));
    boxed.add_child(Box::new(Text::new("boxed", 0, 0, None)));
    assert_snapshot("box_with_background", &boxed.render(30));
}

// --- Loader (upstream components/loader.ts) ---------------------------------

/// Fixed frame 0 of the default spinner: built with a one-hour frame
/// interval, so no tick can land before `stop()` and the frame is pinned
/// deterministically at 0 even under CI load.
#[test]
fn loader_frame0() {
    let mut loader = Loader::new_with_interval(
        RenderHandle::new(|| {}),
        cyan,
        std::string::ToString::to_string,
        "Loading…",
        3_600_000, // 1h in ms: the first tick is a long way off
    );
    loader.stop();
    assert_snapshot("loader_frame0", &loader.render(40));
}

/// Single custom frame: no animation thread at all (frames.len() <= 1),
/// rendered verbatim (loader.ts `renderIndicatorVerbatim`).
#[test]
fn loader_custom_frame() {
    let loader = Loader::new(
        RenderHandle::new(|| {}),
        cyan,
        std::string::ToString::to_string,
        "static indicator",
        Some(LoaderIndicatorOptions {
            frames: Some(vec!["⏳".to_string()]),
            interval_ms: None,
        }),
    );
    assert_snapshot("loader_custom_frame", &loader.render(40));
}
