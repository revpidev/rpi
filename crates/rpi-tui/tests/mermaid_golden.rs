//! Three-way golden check for the grok-build mermaid port
//! (`rpi_tui::mermaid`): rpi output vs grok-mermaid 0.2.2 output vs the
//! grok-build upstream tests (T29). The `.txt` files under
//! `tests/fixtures/mermaid/` were generated with grok-mermaid 0.2.2
//! (TypeScript, Apache-2.0); see the README there for the generator.
//!
//! grok-mermaid 0.2.2 is a newer independent port of grok-build and
//! supports more diagram kinds; layout differences from the pinned
//! grok-build revision are recorded per case below.

use rpi_tui::mermaid::{render, MermaidStyles, Style};

fn neutral_styles() -> MermaidStyles {
    MermaidStyles {
        border: Style::default(),
        node_text: Style::default(),
        edge: Style::default(),
        edge_label: Style::default(),
        title: Style::default(),
    }
}

fn render_plain(src: &str) -> String {
    let art = render(src, &neutral_styles(), None).expect("non-blank source renders");
    format!("{}\n", art.plain_lines.join("\n"))
}

fn case(name: &str) -> (String, String) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mermaid");
    let source = std::fs::read_to_string(format!("{dir}/{name}.md")).expect("source fixture");
    let golden = std::fs::read_to_string(format!("{dir}/{name}.txt")).expect("golden fixture");
    (source, golden)
}

#[test]
fn flowchart_matches_grok_mermaid_golden() {
    let (source, golden) = case("flowchart");
    assert_eq!(render_plain(&source), golden);
}

#[test]
fn sequence_matches_grok_mermaid_golden() {
    let (source, golden) = case("sequence");
    assert_eq!(render_plain(&source), golden);
}

#[test]
fn state_matches_grok_mermaid_golden() {
    let (source, golden) = case("state");
    assert_eq!(render_plain(&source), golden);
}

#[test]
fn unsupported_gantt_matches_grok_mermaid_source_box() {
    // grok-mermaid's `render()` returns null for gantt and the caller draws
    // `sourceBox(src, 80)`; grok-build's `render()` boxes the source
    // itself (rpi-tui `fallback`), which is what this golden pins.
    let (source, golden) = case("unsupported_gantt");
    assert_eq!(render_plain(&source), golden);
}
