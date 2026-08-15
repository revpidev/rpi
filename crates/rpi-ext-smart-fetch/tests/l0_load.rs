//! Host-integration smoke (design §5.5): the real cdylib through the actual
//! rpi host (`NativeExtensionHost`) — capability denial (missing `tools`),
//! full-manifest load with `web_fetch` registered, schema visibility, and a
//! tool execution against the local mock responder (error passthrough +
//! result shape). One `#[tokio::test]` per binary: loads share process env.

mod common;

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
        "librpi_ext_smart_fetch.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_smart_fetch.dll"
    } else {
        "librpi_ext_smart_fetch.so"
    };
    target.join("debug").join(name)
}

fn require_plugin() -> Option<PathBuf> {
    let plugin = plugin_path();
    if plugin.is_file() {
        return Some(plugin);
    }
    eprintln!(
        "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-smart-fetch`",
        plugin.display()
    );
    None
}

fn package(tag: &str, capabilities: &str, plugin: &Path) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-sf-l0-{tag}-{}-{}",
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
            r#"{{"name":"rpi-smart-fetch","version":"0.1.0","native":"dist/{}","capabilities":{},"rpiAbi":1}}"#,
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
    let sandbox = std::env::temp_dir().join(format!("rpi-sf-l0-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join("proj/.rpi")).unwrap();
    std::fs::create_dir_all(sandbox.join("agent")).unwrap();
    // Safety of set_var in tests: this is the only test in this binary.
    unsafe {
        std::env::set_var("RPI_CODING_AGENT_DIR", sandbox.join("agent"));
    }

    // 1. Without the `tools` capability registerTool is denied → load fails.
    let denied_package = package("denied", r#"["session"]"#, &plugin);
    let host = NativeExtensionHost::new(sandbox.join("proj").to_string_lossy().as_ref());
    let errors = host.load_paths(std::slice::from_ref(&denied_package)).await;
    assert!(
        errors
            .iter()
            .any(|error| error.error.contains("capabilityDenied")),
        "expected capabilityDenied load error, got {errors:?}"
    );
    let _ = std::fs::remove_dir_all(denied_package.parent().unwrap());

    // 2. The manifest surface loads clean and registers web_fetch.
    let package = package("full", r#"["tools","session"]"#, &plugin);
    let host = NativeExtensionHost::new(sandbox.join("proj").to_string_lossy().as_ref());
    let errors = host.load_paths(&[package]).await;
    assert!(errors.is_empty(), "{errors:?}");
    let definition = host
        .get_tool_definition("web_fetch")
        .expect("web_fetch registered");

    // FR-P0-1: description carries the upstream sentences verbatim.
    let description = definition.description.as_str();
    assert!(
        description.contains("browser-grade TLS fingerprinting"),
        "description registered: {description}"
    );
    assert!(description.contains("Does NOT execute JavaScript"));

    // Schema visibility: the five format literals and the parameter surface.
    let schema = serde_json::to_value(&definition.parameters).unwrap_or_default();
    let properties = schema.get("properties").cloned().unwrap_or_default();
    for field in [
        "url",
        "browser",
        "os",
        "headers",
        "maxChars",
        "timeoutMs",
        "format",
        "removeImages",
        "includeReplies",
        "proxy",
        "verbose",
    ] {
        assert!(
            properties.get(field).is_some(),
            "schema field {field} missing: {properties}"
        );
    }
    assert_eq!(
        schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|r| r.len()),
        Some(1)
    );

    // 3. Execution against the local responder: result JSON shape + headers.
    let server = common::Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        "plain body from mock".to_string(),
    )]);
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-smoke".to_owned(),
        params: serde_json::json!({ "url": server.url("/doc") }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("web_fetch executes");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect::<String>();
    assert!(text.contains("> URL: http://127.0.0.1:"), "{text}");
    assert!(text.contains("plain body from mock"), "{text}");
    assert!(text.contains("> Browser: chrome_145/windows"), "{text}");
    // details carry the terminal status (P0: no streaming updates).
    let details = result.details.clone();
    assert_eq!(details.get("status"), Some(&serde_json::json!("done")));
    assert_eq!(details.get("progress"), Some(&serde_json::json!(1)));
    assert!(
        details
            .get("fetchResult")
            .and_then(|r| r.get("wordCount"))
            .is_some(),
        "fetchResult shape: {details}"
    );

    // 4. Error passthrough: invalid URL → the upstream error text, no panic.
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-smoke-err".to_owned(),
        params: serde_json::json!({ "url": "not a url" }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("execute returns normally for fetch errors");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect::<String>();
    assert_eq!(text, "Error: Invalid URL: not a url");
    // upstream returns fetch errors as normal tool results (no error flag on
    // the AgentToolResult itself — the error rides the content text)
    assert_eq!(
        result.details.get("status"),
        Some(&serde_json::json!("error"))
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}
