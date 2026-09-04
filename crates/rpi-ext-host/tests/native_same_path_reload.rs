//! Same-path native plugin reload regression (the `/resume` status-line
//! loss bug). Session replacement (`/resume` `/new` `/fork` `/clone`
//! `/import`) builds a FRESH `NativeExtensionHost` which re-loads the same
//! cdylib path; dlopen memoizes per path, so `rpi_extension_init` runs a
//! second time on the SAME plugin statics. Before the rebind fix the
//! mcp-adapter's `install` failed with `init: plugin already initialized`
//! on the second host — the extension (tools, flags, event handlers, the
//! "🔌 MCP" footer entry) silently vanished after `/resume`. This test
//! pins the host-visible half: a second host loading the same path must
//! get a successful init with its own registrations.
//!
//! Packaging mirrors multi_native_plugins_load.rs; skips when the cdylib
//! is missing (build first: `cargo build -p rpi-ext-mcp-adapter`).

use std::path::{Path, PathBuf};

use rpi_ext_host::host::NativeExtensionHost;

fn target_dir() -> PathBuf {
    std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        })
}

/// One packaged plugin: temp dir + crate manifest + the cdylib from
/// target/debug.
fn package(tag: &str, spec: &str, plugin_root: &Path) -> Option<PathBuf> {
    let so = target_dir()
        .join("debug")
        .join(format!("librpi_ext_{}.so", spec.replace('-', "_")));
    if !so.is_file() {
        eprintln!(
            "skipping: cdylib missing at {} — build the plugin crates first",
            so.display()
        );
        return None;
    }
    let manifest = plugin_root.join(format!("crates/rpi-ext-{spec}/rpi-extension.json"));
    let dir = std::env::temp_dir().join(format!(
        "rpi-samepath-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp package dir");
    std::fs::copy(&so, dir.join(so.file_name().unwrap())).expect("copy cdylib");
    std::fs::copy(&manifest, dir.join("rpi-extension.json")).expect("copy manifest");
    Some(dir)
}

/// Two hosts, one plugin path: the second load (session replacement) must
/// succeed and re-register flags + event handlers on the new host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_host_reloading_same_path_reinstalls_the_plugin() {
    let plugin_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let Some(dir) = package("x", "mcp-adapter", &plugin_root) else {
        return;
    };

    // Host 1: like process start.
    let host1 = NativeExtensionHost::new("/tmp");
    let errors1 = host1.load_paths(std::slice::from_ref(&dir)).await;
    assert!(errors1.is_empty(), "{errors1:?}");
    assert!(
        host1.has_handlers("session_start"),
        "host1 must get the mcp event handlers"
    );
    assert!(
        host1.get_tool_definition("mcp").is_some(),
        "host1 must get the mcp proxy tool"
    );

    // Host 2: like /resume — fresh host, same plugin path. Before the
    // rebind fix this failed with "init: plugin already initialized".
    let host2 = NativeExtensionHost::new("/tmp");
    let errors2 = host2.load_paths(std::slice::from_ref(&dir)).await;
    assert!(errors2.is_empty(), "second-host load failed: {errors2:?}");
    assert!(
        host2.has_handlers("session_start"),
        "host2 must get the mcp event handlers re-registered"
    );
    assert!(
        host2.get_tool_definition("mcp").is_some(),
        "host2 must get the mcp proxy tool re-registered"
    );
    assert!(
        host2
            .get_flags()
            .iter()
            .any(|(name, _)| name == "mcp-config"),
        "host2 must get the mcp-config flag re-registered"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
