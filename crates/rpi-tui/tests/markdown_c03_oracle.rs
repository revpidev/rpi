//! C03 target-output oracle (V19-02 §4 / §7.1): the frozen bytes V19-04 must
//! preserve. Cases mirror the upstream behavior checked inline in
//! `components::markdown::tests`: hidden/incomplete HTML comments, a bare URL
//! stopping before CJK punctuation, overwide display math falling back to the
//! raw source, narrow blockquotes dropping the border, and SGR-safe wrapping.
//!
//! Regenerate only for an intentional upstream behavior change:
//!
//! ```sh
//! RPI_UPDATE_SNAPSHOTS=1 cargo test -p rpi-tui --test markdown_c03_oracle
//! ```

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use rpi_tui::components::markdown::{Markdown, MarkdownTheme, ThemeTextFn};
use rpi_tui::terminal_image::{TerminalCapabilities, reset_capabilities_cache, set_capabilities};
use rpi_tui::tui::Component;

fn style(code: &'static str, close: &'static str) -> ThemeTextFn {
    Box::new(move |text: &str| format!("\x1b[{code}m{text}\x1b[{close}m"))
}

fn theme() -> Arc<MarkdownTheme> {
    Arc::new(MarkdownTheme {
        heading: style("36", "0"),
        link: style("34", "0"),
        link_url: style("2", "22"),
        code: style("33", "0"),
        code_block: style("32", "0"),
        code_block_border: style("2", "22"),
        quote: style("3", "23"),
        quote_border: style("2", "22"),
        hr: style("2", "22"),
        list_bullet: style("36", "0"),
        bold: style("1", "22"),
        italic: style("3", "23"),
        strikethrough: style("9", "29"),
        underline: style("4", "24"),
        highlight_code: None,
        code_block_indent: None,
    })
}

fn render(source: &str, width: usize) -> String {
    let markdown = Markdown::new(source, 0, 0, theme(), None, None);
    markdown.render(width).join("\n")
}

fn caps(hyperlinks: bool) -> TerminalCapabilities {
    TerminalCapabilities {
        images: None,
        true_color: false,
        hyperlinks,
    }
}

#[test]
fn markdown_c03_target_output_matches_the_frozen_oracle() {
    let mut actual = String::new();
    let mut case = |name: &str, width: usize, source: &str| {
        actual.push_str(&format!("case: {name} width={width}\n"));
        actual.push_str(&render(source, width));
        actual.push_str("\n---\n");
    };

    case(
        "html-comment-closed",
        80,
        "before\n\n<!-- hidden -->\n\nafter",
    );
    case("html-comment-unclosed", 80, "<!-- streamed");
    case("html-comment-inline", 80, "a <!-- hidden --> b");

    set_capabilities(caps(true));
    case("bare-url-cjk", 120, "https://example.com/path，next");
    reset_capabilities_cache();

    case(
        "display-math-wide",
        80,
        r"$$\begin{matrix}a&b\\c&d\end{matrix}$$",
    );
    case(
        "display-math-narrow",
        3,
        r"$$\begin{matrix}a&b\\c&d\end{matrix}$$",
    );
    case("narrow-blockquote-2", 2, "> xy");
    case("narrow-blockquote-3", 3, "> xy");
    case("sgr-wrap", 10, "**bold words** and `code` wrap");

    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/markdown_c03_oracle.snap");
    if std::env::var_os("RPI_UPDATE_SNAPSHOTS").is_some() {
        fs::write(&path, &actual).unwrap_or_else(|error| panic!("write {path:?}: {error}"));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("read {path:?}: {error}; run with RPI_UPDATE_SNAPSHOTS=1 to create it")
    });
    assert_eq!(
        actual, expected,
        "C03 oracle changed; V19-04 must preserve these bytes (regenerate only for an intentional upstream change)"
    );
}
