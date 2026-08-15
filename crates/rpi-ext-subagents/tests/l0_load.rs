//! L0 load smoke (design §5.4): the real cdylib through the actual host —
//! capability denial (missing `tools`) and the full-manifest load with the
//! `subagent` tool registered. One `#[tokio::test]` per binary: loads share
//! the process env.

use std::path::{Path, PathBuf};

use rpi_ext_host::host::NativeExtensionHost;

fn plugin_path() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    let name = if cfg!(target_os = "macos") {
        "librpi_ext_subagents.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_subagents.dll"
    } else {
        "librpi_ext_subagents.so"
    };
    target.join("debug").join(name)
}

fn require_plugin() -> Option<PathBuf> {
    let plugin = plugin_path();
    if plugin.is_file() {
        return Some(plugin);
    }
    eprintln!(
        "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-subagents` (or run `cargo test --workspace`)",
        plugin.display()
    );
    None
}

/// Package the cdylib with a manifest carrying the given capabilities.
fn package(tag: &str, capabilities: &str, plugin: &Path) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-sub-l0-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let plugin_dir = dir.join("plug");
    std::fs::create_dir_all(plugin_dir.join("dist")).expect("dist");
    let plugin_name = plugin.file_name().expect("plugin file name");
    std::fs::copy(plugin, plugin_dir.join("dist").join(plugin_name)).expect("copy");
    std::fs::write(
        plugin_dir.join("rpi-extension.json"),
        format!(
            r#"{{"name":"rpi-subagents","version":"0.1.0","native":"dist/{}","capabilities":{},"rpiAbi":1}}"#,
            plugin_name.to_string_lossy(),
            capabilities,
        ),
    )
    .expect("manifest");
    plugin_dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l0_load_capability_denied_and_full_surface() {
    let Some(plugin) = require_plugin() else {
        return;
    };
    let sandbox = std::env::temp_dir().join(format!("rpi-sub-l0-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join("proj/.rpi")).unwrap();
    std::fs::create_dir_all(sandbox.join("agent")).unwrap();
    // Isolate the loaded plugin's discovery from the developer's real ~/.rpi.
    // Safety of set_var in tests: this is the only test in this binary.
    unsafe {
        std::env::set_var("RPI_CODING_AGENT_DIR", sandbox.join("agent"));
        std::env::set_var("RPI_SUBAGENT_RPI_BINARY", "/nonexistent-rpi");
    }

    // 1. Without the `tools` capability the registerTool host call is denied
    //    and init fails the load with capabilityDenied.
    let denied_package = package("denied", r#"["commands","session"]"#, &plugin);
    let host = NativeExtensionHost::new(sandbox.join("proj").to_string_lossy().as_ref());
    let errors = host.load_paths(std::slice::from_ref(&denied_package)).await;
    assert!(
        errors
            .iter()
            .any(|error| error.error.contains("capabilityDenied")),
        "expected capabilityDenied load error, got {errors:?}"
    );
    let _ = std::fs::remove_dir_all(denied_package.parent().unwrap());

    // 2. The real manifest surface loads clean and registers the tool
    //    (TE09: `ui` joins the capabilities for registerMessageRenderer).
    let package = package(
        "full",
        r#"["tools","commands","session","events","ui"]"#,
        &plugin,
    );
    let host = NativeExtensionHost::new(sandbox.join("proj").to_string_lossy().as_ref());
    let errors = host.load_paths(&[package]).await;
    assert!(errors.is_empty(), "{errors:?}");
    let definition = host
        .get_tool_definition("subagent")
        .expect("subagent tool registered");
    let description = definition.description.as_str();
    assert!(
        description.contains("SINGLE RUN"),
        "full tool description registered: {description}"
    );
    assert!(description.contains("SAFETY-CRITICAL SUBAGENT GUIDANCE"));

    // The registered execute path drives the management surface through the
    // host context (cwd = the session cwd the host was created with).
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-smoke".to_owned(),
        params: serde_json::json!({ "action": "list" }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("list executes");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect::<String>();
    for name in [
        "delegate",
        "oracle",
        "researcher",
        "reviewer",
        "scout",
        "worker",
    ] {
        assert!(text.contains(&format!("- {name} (builtin")), "{text}");
    }

    let _ = std::fs::remove_dir_all(&sandbox);
}
