//! renderCall parity: rpi (Rust) leg (TE09 FR-E).
//!
//! Reads the shared fixture JSON (the same file `render-call-upstream.mjs`
//! reads), runs this crate's `render::format_mcp_proxy_tool_call_lines` /
//! `format_mcp_direct_tool_call_lines` / render wrappers over each case, and
//! prints one JSON document per line — byte-comparable with the upstream
//! leg. The render cases extract the ComponentTree text lines (the ANSI-free
//! counterpart of the upstream plain-theme `Text.render(80)`).
//!
//!   cargo run --example render_call_parity -- <fixtures.json>
//!
//! Spawned only by `scripts/mcp-parity/run-render-call-parity.mjs`; never
//! part of `cargo test`.

use serde_json::Value;

fn extract_tree_text(tree: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for child in tree["children"].as_array().unwrap_or(&Vec::new()) {
        if let Some(text) = child["props"]["text"].as_str() {
            lines.push(text.to_string());
        }
    }
    lines.join("\n")
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        let manifest = env!("CARGO_MANIFEST_DIR");
        format!("{manifest}/../../scripts/mcp-parity/render-call-fixtures.json")
    });
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixtures {path}: {e}"));
    let cases = serde_json::from_str::<Value>(&raw)
        .expect("fixtures JSON")
        .get("cases")
        .and_then(Value::as_array)
        .expect("cases array")
        .clone();

    for item in cases {
        let name = item["name"].as_str().unwrap_or_default().to_string();
        match item["kind"].as_str() {
            Some("proxy") => {
                let lines = rpi_ext_mcp_adapter::render::format_mcp_proxy_tool_call_lines(
                    &item["args"],
                    rpi_ext_mcp_adapter::render::DEFAULT_MAX_CALL_INPUT_CHARS,
                );
                println!("{}", serde_json::json!({ "name": name, "lines": lines }));
            }
            Some("direct") => {
                let lines = rpi_ext_mcp_adapter::render::format_mcp_direct_tool_call_lines(
                    item["displayName"].as_str().unwrap_or_default(),
                    &item["args"],
                    rpi_ext_mcp_adapter::render::DEFAULT_MAX_CALL_INPUT_CHARS,
                );
                println!("{}", serde_json::json!({ "name": name, "lines": lines }));
            }
            Some("render-proxy") => {
                let tree = rpi_ext_mcp_adapter::render::render_mcp_proxy_tool_call(&item["args"]);
                println!(
                    "{}",
                    serde_json::json!({ "name": name, "rendered": extract_tree_text(&tree) })
                );
            }
            Some("render-direct") => {
                let tree = rpi_ext_mcp_adapter::render::render_mcp_direct_tool_call(
                    item["displayName"].as_str().unwrap_or_default(),
                    &item["args"],
                );
                println!(
                    "{}",
                    serde_json::json!({ "name": name, "rendered": extract_tree_text(&tree) })
                );
            }
            _ => {}
        }
    }
}
