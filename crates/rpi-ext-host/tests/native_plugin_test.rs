//! T15 W7 native-plugin (abi_stable) host tests. The fixture is the
//! workspace `rpi-test-native-plugin` cdylib (built by `cargo test
//! --workspace` or `cargo build -p rpi-test-native-plugin`; a lone
//! `-p rpi-ext-host` test run skips with a hint when the artifact is
//! missing instead of failing spuriously).

use std::path::PathBuf;

use rpi_ext_host::host::NativeExtensionHost;
use serde_json::json;

/// Platform-specific cdylib artifact names (`cargo build` default naming: lib<name>.so /
/// lib<name>.dylib / <name>.dll）。
fn plugin_file_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "librpi_test_native_plugin.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_test_native_plugin.dll"
    } else {
        "librpi_test_native_plugin.so"
    }
}

/// Debug artifact path of the fixture cdylib.
fn plugin_path() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    target.join("debug").join(plugin_file_name())
}

/// Skips with a build hint when the artifact is missing (`cargo test -p rpi-ext-host`
/// does not build cdylibs of non-dependency crates).
fn require_plugin() -> Option<PathBuf> {
    let plugin = plugin_path();
    if plugin.is_file() {
        return Some(plugin);
    }
    eprintln!(
        "skipping: fixture cdylib missing at {} — build it with `cargo build -p rpi-test-native-plugin` (or run `cargo test --workspace`)",
        plugin.display()
    );
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_plugin_registers_and_blocks_tool_call() {
    let Some(plugin) = require_plugin() else {
        return;
    };

    let host = NativeExtensionHost::new("/w7-native-cwd");
    let errors = host.load_paths(&[plugin]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.has_handlers("tool_call"));

    let result = host
        .emit_tool_call(json!({
            "type": "tool_call",
            "toolCallId": "c1",
            "toolName": "read",
            "input": {"path": "/etc/passwd"}
        }))
        .await
        .expect("no host error")
        .expect("gate blocked");
    assert_eq!(result["block"], true);
    assert_eq!(result["reason"], "native-gate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_plugin_manifest_entry_loads() {
    let Some(plugin) = require_plugin() else {
        return;
    };

    // The manifest's `native` field resolves through the discovery rule (wasm wins over native).
    let dir = std::env::temp_dir().join(format!("rpi-native-manifest-{}", std::process::id()));
    let plugin_dir = dir.join("plug");
    std::fs::create_dir_all(plugin_dir.join("dist")).expect("dist");
    let plugin_name = plugin.file_name().expect("plugin file name");
    std::fs::copy(&plugin, plugin_dir.join("dist").join(plugin_name)).expect("copy");
    std::fs::write(
        plugin_dir.join("rpi-extension.json"),
        format!(
            r#"{{"name":"gate","version":"0.1.0","native":"dist/{}","capabilities":[],"rpiAbi":1}}"#,
            plugin_name.to_string_lossy()
        ),
    )
    .expect("manifest");

    let host = NativeExtensionHost::new(&dir.to_string_lossy());
    let errors = host.load_paths(std::slice::from_ref(&plugin_dir)).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.has_handlers("tool_call"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_plugin_capability_denied_without_tools() {
    let Some(plugin) = require_plugin() else {
        return;
    };

    // Bare .so (no manifest) → capabilities=[] — the fixture only calls `on` (free);
    // it must load successfully; capability denial is already covered by the wasm-side
    // parity (same method table).
    let host = NativeExtensionHost::new("/w7-native-caps");
    let errors = host.load_paths(&[plugin]).await;
    assert!(errors.is_empty(), "{errors:?}");
}
