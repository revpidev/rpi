//! TE06 extraction baseline runner (design §5.2): runs the dom_smoothie
//! engine over the frozen corpus (`fixtures/smart-fetch-corpus/`) and prints
//! one JSON record per page for the metric harness
//! (`scripts/smart-fetch-parity/extract-metrics.mjs`).
//!
//! Run: cargo run -p rpi-ext-smart-fetch --example extract_baseline

use std::path::Path;

use rpi_ext_smart_fetch::extract::{DomSmoothieExtractor, ExtractOptions, Extractor};
use rpi_ext_smart_fetch::types::IncludeReplies;

fn main() {
    let corpus_dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/smart-fetch-corpus"
    );
    let mut entries: Vec<_> = std::fs::read_dir(corpus_dir)
        .expect("corpus directory readable — run gen-corpus.mjs first")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("html"))
        .collect();
    entries.sort();

    let engine = DomSmoothieExtractor;
    println!("[");
    let mut records = Vec::new();
    for path in &entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let html = std::fs::read_to_string(path).unwrap_or_default();
        let url = format!("https://corpus.example.com/{name}");
        let extracted = engine.extract(
            &html,
            &url,
            &ExtractOptions {
                markdown: true,
                remove_images: false,
                include_replies: IncludeReplies::Extractors,
            },
        );
        records.push(serde_json::json!({
            "name": name,
            "title": extracted.title,
            "author": extracted.author,
            "published": extracted.published,
            "site": extracted.site,
            "language": extracted.language,
            "content": extracted.content,
        }));
    }
    let _ = Path::new(""); // keep import used if refactored
    for (index, record) in records.iter().enumerate() {
        let comma = if index + 1 < records.len() { "," } else { "" };
        println!("{record}{comma}");
    }
    println!("]");
}
