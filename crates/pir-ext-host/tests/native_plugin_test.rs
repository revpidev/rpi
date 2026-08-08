//! T15 W7 native-plugin (abi_stable) host tests. The fixture is the
//! workspace `pir-test-native-plugin` cdylib (built by `cargo test
//! --workspace` or `cargo build -p pir-test-native-plugin`; a lone
//! `-p pir-ext-host` test run skips with a hint when the artifact is
//! missing instead of failing spuriously).

use std::path::PathBuf;

use pir_ext_host::host::NativeExtensionHost;
use serde_json::json;

/// 平台化的 cdylib 产物名（`cargo build` 的默认命名：lib<name>.so /
/// lib<name>.dylib / <name>.dll）。
fn plugin_file_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libpir_test_native_plugin.dylib"
    } else if cfg!(target_os = "windows") {
        "pir_test_native_plugin.dll"
    } else {
        "libpir_test_native_plugin.so"
    }
}

/// Fixture cdylib 的 debug 产物路径。
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

/// 产物缺失（`cargo test -p pir-ext-host` 不构建非依赖 crate 的 cdylib）
/// 时跳过并给出构建指引。
fn require_plugin() -> Option<PathBuf> {
    let plugin = plugin_path();
    if plugin.is_file() {
        return Some(plugin);
    }
    eprintln!(
        "skipping: fixture cdylib missing at {} — build it with `cargo build -p pir-test-native-plugin` (or run `cargo test --workspace`)",
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

    // manifest 的 `native` 字段经发现规则解析（wasm 优先于 native）。
    let dir = std::env::temp_dir().join(format!("pir-native-manifest-{}", std::process::id()));
    let plugin_dir = dir.join("plug");
    std::fs::create_dir_all(plugin_dir.join("dist")).expect("dist");
    let plugin_name = plugin.file_name().expect("plugin file name");
    std::fs::copy(&plugin, plugin_dir.join("dist").join(plugin_name)).expect("copy");
    std::fs::write(
        plugin_dir.join("pir-extension.json"),
        format!(
            r#"{{"name":"gate","version":"0.1.0","native":"dist/{}","capabilities":[],"pirAbi":1}}"#,
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

    // 裸 .so（无 manifest）→ capabilities=[] —— fixture 只调 `on`（免费），
    // 应加载成功；capability 拒绝已由 wasm 侧对拍覆盖（同一 method 表）。
    let host = NativeExtensionHost::new("/w7-native-caps");
    let errors = host.load_paths(&[plugin]).await;
    assert!(errors.is_empty(), "{errors:?}");
}
