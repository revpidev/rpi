//! stdio transport tests, ported from `packages/mcp/test/stdio.test.ts`
//! @ a13d35a74. The fixture server is `tests/fixtures/mcp-test-server.rs`.

use std::time::Duration;

use rpi_mcp::transport::stdio::StdioTransportOptions;
use rpi_mcp::{
    LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions, McpRequestOptions, StdioTransport,
};

fn fixture_command() -> String {
    env!("CARGO_BIN_EXE_mcp-test-server").to_owned()
}

#[tokio::test]
async fn connects_to_a_newline_delimited_server_and_captures_stderr() {
    let stderr_chunks: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = std::sync::Arc::new(StdioTransport::new(StdioTransportOptions {
        on_stderr: Some({
            let stderr_chunks = stderr_chunks.clone();
            std::sync::Arc::new(move |chunk: &str| {
                stderr_chunks.lock().unwrap().push(chunk.to_owned());
            })
        }),
        ..StdioTransportOptions::new(fixture_command())
    }));
    let client = McpClient::new(McpClientOptions::new("stdio-test", "1.0.0"));
    client.connect(transport.clone()).await.unwrap();
    assert_eq!(
        client.protocol_version().as_deref(),
        Some(LATEST_PROTOCOL_VERSION)
    );
    let tools = client
        .list_tools(McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool(
            "echo",
            Some(serde_json::json!({"text": "hello"})),
            McpRequestOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.content[0]["text"], "hello");
    assert!(transport.pid().is_some());
    for _ in 0..100 {
        if transport.stderr().contains("stdio fixture ready") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        transport.stderr().contains("stdio fixture ready"),
        "{}",
        transport.stderr()
    );
    assert!(
        stderr_chunks
            .lock()
            .unwrap()
            .join("")
            .contains("stdio fixture ready")
    );
    client.close().await.unwrap();
    assert_eq!(client.connection_state(), rpi_mcp::ClientState::Closed);
}

/// The stubborn server ignores SIGTERM and stdin EOF and spawns a grandchild;
/// closing must kill the whole process group (G4 red line).
#[cfg(unix)]
#[tokio::test]
async fn kills_a_server_that_ignores_shutdown_including_its_children() {
    let mut options = StdioTransportOptions::new(fixture_command());
    options.args = vec!["--stubborn".to_owned()];
    options.close_timeout_ms = Some(100);
    let transport = std::sync::Arc::new(StdioTransport::new(options));
    let client = McpClient::new(McpClientOptions::new("stdio-test", "1.0.0"));
    client.connect(transport.clone()).await.unwrap();
    let mut grandchild: Option<i32> = None;
    for _ in 0..200 {
        if let Some(pid) = transport.stderr().lines().find_map(|line| {
            line.strip_prefix("grandchild ")
                .and_then(|value| value.trim().parse().ok())
        }) {
            grandchild = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let grandchild = grandchild.expect("grandchild pid in stderr");
    let started = std::time::Instant::now();
    client.close().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut alive = true;
    for _ in 0..100 {
        // SAFETY: signal 0 only probes whether the process exists.
        let exists = unsafe { libc::kill(grandchild, 0) } == 0;
        if !exists {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive, "grandchild {grandchild} still alive");
}
