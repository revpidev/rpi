//! TE52 G12/G7: the shipped manifest pins the capability surface the plugin
//! uses (commands + ui + session + events), the ABI major and the native
//! carrier. The lockstep `version`/`minHostVersion` pair is injected at pack
//! time (build.yml rewrites the packed copy with the workspace version), so
//! the source-tree manifest only pins the capability/ABI shape.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn crate_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

#[test]
fn manifest_pins_capabilities_abi_and_native_only_carrier() {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(crate_file("rpi-extension.json")).expect("manifest"),
    )
    .expect("manifest json");
    assert_eq!(
        manifest["capabilities"],
        serde_json::json!(["commands", "ui", "session", "events"]),
        "the editor needs commands; items need ui/session/events"
    );
    assert_eq!(manifest["rpiAbi"], serde_json::json!(1), "rpiAbi: 1");
    assert!(manifest.get("wasm").is_none(), "native-only carrier");
    assert_eq!(manifest["native"], "librpi_ext_statusline.so");
    assert_eq!(manifest["name"], "rpi-statusline");
    assert!(
        manifest["description"]
            .as_str()
            .is_some_and(|text| text.contains("items") && text.contains("/statusline")),
        "description names the items mode and the editor"
    );
}

#[test]
fn production_modules_are_file_backed_and_reachable() {
    let lib = std::fs::read_to_string(crate_file("src/lib.rs")).expect("lib.rs");
    for module in [
        "config",
        "editor",
        "fields",
        "git",
        "items",
        "paths",
        "payload",
        "refresh",
        "render",
        "runner",
        "runtime",
        "session_stats",
        "settings_write",
        "state",
        "subscriptions",
    ] {
        assert!(
            lib.contains(&format!("pub mod {module};")),
            "lib.rs declares {module}"
        );
        assert!(
            crate_file(&format!("src/{module}.rs")).is_file(),
            "src/{module}.rs exists"
        );
    }
    assert!(
        !crate_file("src/mod.rs").exists(),
        "the crate must not contain mod.rs"
    );
}
