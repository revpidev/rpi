//! Golden parity: compact `mcp-cache.json` write (TE25 FR-B, R7.2.12.2 / #395).
//!
//! `fixtures/cache_compact_cases.json`'s `compact` string is produced by
//! Node's `JSON.stringify(merged)` — exactly the upstream
//! `saveMetadataCache` write call at `metadata-cache.ts:78 @ 10a45367`.
//! The test asserts both directions:
//!   1. upstream bytes → plugin read: `load_metadata_cache` parses the exact
//!      upstream bytes into the typed cache;
//!   2. plugin write → upstream bytes: `save_metadata_cache` reproduces the
//!      upstream bytes byte-for-byte (key order, no indentation, no trailing
//!      newline).

use rpi_ext_mcp_adapter::cache::{
    load_metadata_cache, save_metadata_cache, MetadataCache, CACHE_VERSION,
};
use serde_json::Value;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-cache-compact-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn cache_compact_write_matches_upstream_bytes() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/cache_compact_cases.json"))
        .expect("fixture parses");
    let compact = fixture["compact"].as_str().expect("compact string");
    let merged: MetadataCache =
        serde_json::from_value(fixture["merged"].clone()).expect("merged cache parses");
    assert_eq!(merged.version, CACHE_VERSION);
    assert!(
        !compact.contains('\n'),
        "upstream bytes must be the compact JSON.stringify shape"
    );

    let dir = temp_dir("golden");

    // 1) upstream → plugin.
    let read_path = dir.join("mcp-cache.json");
    std::fs::write(&read_path, compact).expect("write upstream bytes");
    let loaded = load_metadata_cache(&read_path).expect("upstream compact cache parses");
    assert_eq!(loaded.servers, merged.servers);

    // 2) plugin → upstream (byte-for-byte).
    let write_path = dir.join("written").join("mcp-cache.json");
    save_metadata_cache(&write_path, &merged).expect("save");
    let written = std::fs::read_to_string(&write_path).expect("raw written cache");
    assert_eq!(written, compact);
    assert!(!written.contains('\n'));

    let _ = std::fs::remove_dir_all(&dir);
}
