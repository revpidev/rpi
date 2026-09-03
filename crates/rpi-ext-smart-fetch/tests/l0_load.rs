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
    // both packaged copies (cdylib ≈ .5 GB with debug info) are cleaned up —
    // leaked copies once filled /tmp
    let _ = std::fs::remove_dir_all(denied_package.parent().unwrap());

    // 2. The manifest surface loads clean and registers web_fetch.
    let package = package("full", r#"["tools","session"]"#, &plugin);
    let host = NativeExtensionHost::new(sandbox.join("proj").to_string_lossy().as_ref());
    let errors = host.load_paths(std::slice::from_ref(&package)).await;
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
    // the AgentToolResult itself — the error rides the content text). The
    // error frame's status/phase echo the LATEST progress state, not a
    // hardcoded error/error (index.ts:576-577 spreads latestDetails): URL
    // validation is a return-path error that never emits a progress event,
    // so the frame keeps the initial connecting/fetch_start state.
    assert_eq!(
        result.details.get("status"),
        Some(&serde_json::json!("connecting"))
    );
    assert_eq!(
        result.details.get("phase"),
        Some(&serde_json::json!("fetch_start"))
    );

    // 5. FR-P1-5: settings are read per execute — global <agentDir> +
    // project `.rpi`, project overriding global per key.
    std::fs::write(
        sandbox.join("agent/settings.json"),
        serde_json::json!({
            "smartFetchDefaultBatchConcurrency": 2,
            "webFetchDefaultMaxChars": 333,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        sandbox.join("proj/.rpi/settings.json"),
        serde_json::json!({ "smartFetchDefaultBatchConcurrency": 3 }).to_string(),
    )
    .unwrap();

    // 6. FR-P1-1: batch_web_fetch registers with the upstream schema and
    // fans out with bounded concurrency + per-item isolation.
    let batch_definition = host
        .get_tool_definition("batch_web_fetch")
        .expect("batch_web_fetch registered");
    let batch_description = batch_definition.description.as_str();
    assert!(
        batch_description.contains("bounded concurrency"),
        "batch description registered: {batch_description}"
    );
    let batch_schema = serde_json::to_value(&batch_definition.parameters).unwrap_or_default();
    let requests_schema = batch_schema
        .get("properties")
        .and_then(|p| p.get("requests"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        requests_schema.get("minItems"),
        Some(&serde_json::json!(1)),
        "requests minItems: {requests_schema}"
    );
    assert_eq!(
        requests_schema
            .get("items")
            .and_then(|i| i.get("additionalProperties")),
        Some(&serde_json::json!(false))
    );

    let server = common::Responder::start(vec![
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/plain".to_string())],
            "batch body one".to_string(),
        ),
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/plain".to_string())],
            "batch body two".to_string(),
        ),
    ]);
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-batch".to_owned(),
        params: serde_json::json!({
            "requests": [
                { "url": server.url("/one") },
                { "url": "not a url" },
                { "url": server.url("/two") },
            ],
        }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    };
    let result = (batch_definition.execute)(request, host.core().create_context())
        .await
        .expect("batch executes");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect::<String>();
    assert!(text.contains("> Requests: 3"), "{text}");
    assert!(text.contains("> Succeeded: 2"), "{text}");
    assert!(text.contains("> Failed: 1"), "{text}");
    // input order kept: [1/3] ok, [2/3] error, [3/3] ok
    let one = text.find("## [1/3]").expect("item 1");
    let error_item = text.find("## [2/3]").expect("item 2");
    let two = text.find("## [3/3]").expect("item 3");
    assert!(one < error_item && error_item < two, "{text}");
    assert!(
        text[error_item..two].contains("Invalid URL: not a url"),
        "{}",
        text
    );
    // project settings override global: concurrency 3, not 2
    assert!(text.contains("> Concurrency: 3"), "{text}");
    // the final progress snapshot rides details (on_update gap, design §1.3 #3)
    let details = result.details.clone();
    assert_eq!(
        details
            .get("batchProgress")
            .and_then(|p| p.get("completed")),
        Some(&serde_json::json!(3)),
        "details: {details}"
    );
    assert_eq!(
        details
            .get("batchProgress")
            .and_then(|p| p.get("batchConcurrency")),
        Some(&serde_json::json!(3))
    );
    assert_eq!(
        details.get("batchResult").and_then(|r| r.get("succeeded")),
        Some(&serde_json::json!(2))
    );

    // 7. global settings apply to web_fetch defaults too (maxChars 333).
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-settings".to_owned(),
        params: serde_json::json!({ "url": server.url("/three") }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("web_fetch executes");
    assert_eq!(
        result.details.get("maxChars"),
        Some(&serde_json::json!(333)),
        "global webFetchDefaultMaxChars applied: {}",
        result.details
    );

    // 8. FR-P2-B/D: toolUpdate streams partial frames (ADR-0015 sink → the
    // on_update callback here). A local mock responder answers in
    // milliseconds, so spinner ticks are not asserted (time-dependent) —
    // the EVENT frames are: fetch_start(connecting) → response_headers
    // (loading 0.51) → body_complete (0.95) → done(1).
    let server = common::Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        "streamed body".to_string(),
    )]);
    let frames: std::sync::Arc<std::sync::Mutex<Vec<rpi_agent::types::AgentToolResult>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let frames_for_update = std::sync::Arc::clone(&frames);
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-stream".to_owned(),
        params: serde_json::json!({ "url": server.url("/doc") }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: Some(Box::new(move |update| {
            frames_for_update.lock().unwrap().push(update);
        })),
    };
    let result = (definition.execute)(request, host.core().create_context())
        .await
        .expect("streaming web_fetch executes");
    // Clone the frames out under a short-lived guard (clippy: never hold a
    // std MutexGuard across the batch await further down).
    let frames: Vec<_> = frames.lock().unwrap().clone();
    assert!(frames.len() >= 3, "event frames arrived: {}", frames.len());
    for frame in frames.iter() {
        let text = frame
            .content
            .iter()
            .map(|block| match block {
                rpi_ai::types::ToolResultContent::Text(text) => text.text.clone(),
                _ => String::new(),
            })
            .collect::<String>();
        assert!(
            text.starts_with("Fetching http://127.0.0.1:"),
            "frame text: {text}"
        );
        assert!(text.ends_with("..."), "frame text: {text}");
    }
    let statuses: Vec<String> = frames
        .iter()
        .map(|frame| {
            frame
                .details
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(statuses.first().map(String::as_str), Some("connecting"));
    assert_eq!(statuses.last().map(String::as_str), Some("done"));
    let mut monotonic = true;
    for pair in frames.windows(2) {
        let a = pair[0].details.get("progress").and_then(|p| p.as_f64());
        let b = pair[1].details.get("progress").and_then(|p| p.as_f64());
        if let (Some(a), Some(b)) = (a, b) {
            monotonic &= b >= a;
        }
    }
    assert!(monotonic, "progress monotonic: {statuses:?}");
    // The terminal frame carries the spinnerTick field (time-dependent
    // value, presence-only assertion).
    assert!(result.details.get("spinnerTick").is_some());

    // 9. FR-P2-C: batch progress snapshots stream through toolUpdate — the
    // initial all-queued frame, per-item frames, and the terminal
    // completed==total snapshot.
    let server = common::Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        "batch streamed".to_string(),
    )]);
    let batch_frames: std::sync::Arc<std::sync::Mutex<Vec<rpi_agent::types::AgentToolResult>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let batch_frames_for_update = std::sync::Arc::clone(&batch_frames);
    let request = rpi_ext_host::types::ToolExecuteRequest {
        tool_call_id: "l0-batch-stream".to_owned(),
        params: serde_json::json!({
            "requests": [
                { "url": server.url("/one") },
                { "url": "not a url" },
            ],
        }),
        signal: tokio_util::sync::CancellationToken::new(),
        on_update: Some(Box::new(move |update| {
            batch_frames_for_update.lock().unwrap().push(update);
        })),
    };
    let result = (batch_definition.execute)(request, host.core().create_context())
        .await
        .expect("streaming batch executes");
    let batch_frames: Vec<_> = batch_frames.lock().unwrap().clone();
    assert!(
        batch_frames.len() >= 3,
        "batch frames arrived: {}",
        batch_frames.len()
    );
    let first_text = batch_frames[0]
        .content
        .iter()
        .map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect::<String>();
    assert_eq!(
        first_text, "Started batch fetch for 2 URLs (0/2 complete).",
        "initial frame text"
    );
    let first_snapshot = batch_frames[0].details.get("batchProgress");
    assert_eq!(
        first_snapshot
            .and_then(|p| p.get("items"))
            .and_then(|items| items.get(0))
            .and_then(|item| item.get("status")),
        Some(&serde_json::json!("queued")),
        "initial all-queued frame"
    );
    let last_snapshot = batch_frames
        .last()
        .unwrap()
        .details
        .get("batchProgress")
        .cloned()
        .unwrap_or_default();
    assert_eq!(last_snapshot.get("completed"), Some(&serde_json::json!(2)));
    assert_eq!(last_snapshot.get("total"), Some(&serde_json::json!(2)));
    assert_eq!(last_snapshot.get("succeeded"), Some(&serde_json::json!(1)));
    assert_eq!(last_snapshot.get("failed"), Some(&serde_json::json!(1)));
    // The final result details keep the TE06/07 snapshot shape + spinnerTick.
    assert!(result.details.get("spinnerTick").is_some());
    assert_eq!(
        result
            .details
            .get("batchProgress")
            .and_then(|p| p.get("completed")),
        Some(&serde_json::json!(2))
    );

    // 10. FR-P2-E: renderCall/renderResult produce ComponentTree v1 JSON
    // through the host's render closure wiring (host_call.rs render arms
    // carry the toolName so the two-tool dispatch resolves).
    let render_call = definition.render_call.as_ref().expect("renderCall wired");
    let context = rpi_ext_host::types::ToolRenderContext {
        args: serde_json::json!({ "url": "https://example.com/x" }),
        tool_call_id: "l0-render".to_owned(),
        cwd: "/tmp".to_owned(),
        execution_started: true,
        args_complete: true,
        is_partial: false,
        expanded: false,
        show_images: false,
        is_error: false,
        terminal_width: None,
    };
    let call_tree = render_call(context).expect("renderCall returns a tree");
    assert_eq!(call_tree["type"], serde_json::json!("text"));
    assert_eq!(
        call_tree["props"]["text"],
        serde_json::json!("web_fetch https://example.com/x")
    );

    let render_result = definition
        .render_result
        .as_ref()
        .expect("renderResult wired");
    let partial_tree = render_result(
        rpi_agent::types::AgentToolResult {
            content: vec![],
            details: serde_json::json!({
                "status": "loading", "progress": 0.51,
                "url": "https://example.com/x", "spinnerTick": 1,
            }),
            usage: None,
            added_tool_names: None,
            terminate: None,
        },
        rpi_ext_host::types::ToolRenderResultOptions {
            expanded: false,
            is_partial: true,
        },
        rpi_ext_host::types::ToolRenderContext {
            args: serde_json::json!({}),
            tool_call_id: "l0-render".to_owned(),
            cwd: "/tmp".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: true,
            expanded: false,
            show_images: false,
            is_error: false,
            terminal_width: None,
        },
    )
    .expect("partial renderResult returns a tree");
    let partial_text = partial_tree["props"]["text"].as_str().unwrap_or_default();
    assert!(
        partial_text.contains('█'),
        "progress bar glyphs: {partial_text}"
    );

    let final_tree = render_result(
        rpi_agent::types::AgentToolResult {
            content: vec![],
            details: serde_json::json!({
                "fetchResult": {
                    "kind": "content",
                    "title": "T",
                    "content": "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9",
                }
            }),
            usage: None,
            added_tool_names: None,
            terminate: None,
        },
        rpi_ext_host::types::ToolRenderResultOptions {
            expanded: false,
            is_partial: false,
        },
        rpi_ext_host::types::ToolRenderContext {
            args: serde_json::json!({}),
            tool_call_id: "l0-render".to_owned(),
            cwd: "/tmp".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: false,
            expanded: false,
            show_images: false,
            is_error: false,
            terminal_width: None,
        },
    )
    .expect("final renderResult returns a tree");
    assert_eq!(final_tree["type"], serde_json::json!("column"));
    let joined = serde_json::to_string(&final_tree).unwrap_or_default();
    assert!(joined.contains("Title: T"), "metadata line: {joined}");
    assert!(
        joined.contains("(2 more lines, Ctrl+O to expand)"),
        "collapse hint: {joined}"
    );

    let batch_render_result = batch_definition
        .render_result
        .as_ref()
        .expect("batch renderResult wired");
    let batch_tree = batch_render_result(
        rpi_agent::types::AgentToolResult {
            content: vec![],
            details: serde_json::json!({
                "batchProgress": {
                    "items": [
                        { "index": 0, "url": "https://ex.com/a", "status": "done", "progress": 1.0 },
                        { "index": 1, "url": "https://ex.com/b", "status": "error", "progress": 1.0, "error": "boom" },
                    ],
                    "total": 2, "completed": 2, "succeeded": 1, "failed": 1,
                    "batchConcurrency": 3,
                },
                "spinnerTick": 0,
            }),
            usage: None,
            added_tool_names: None,
            terminate: None,
        },
        rpi_ext_host::types::ToolRenderResultOptions {
            expanded: true,
            is_partial: false,
        },
        rpi_ext_host::types::ToolRenderContext {
            args: serde_json::json!({}),
            tool_call_id: "l0-render".to_owned(),
            cwd: "/tmp".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: false,
            expanded: true,
            show_images: false,
            is_error: false,
        terminal_width: None,
        },
    )
    .expect("batch renderResult returns a tree");
    let joined = serde_json::to_string(&batch_tree).unwrap_or_default();
    assert!(
        joined.contains("batch_web_fetch 2/2 done · ok 1 · err 1 · concurrency 3"),
        "batch summary: {joined}"
    );
    assert!(
        joined.contains("error: boom"),
        "expanded error line: {joined}"
    );

    let _ = std::fs::remove_dir_all(package.parent().unwrap());
    let _ = std::fs::remove_dir_all(&sandbox);
}
