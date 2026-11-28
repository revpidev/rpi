//! stdio transport tests, ported from `packages/mcp/test/stdio.test.ts`
//! @ b4afdddf. The fixture server is `tests/fixtures/mcp-test-server.rs`.

use std::time::Duration;

#[cfg(unix)]
use rpi_mcp::transport::McpTransport;
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

#[cfg(unix)]
fn is_running(pid: i32) -> bool {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat
            .rsplit_once(") ")
            .is_some_and(|(_, state)| state.starts_with('Z'))
    {
        return false;
    }
    // SAFETY: signal 0 only probes whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
fn kill_group(pid: i32) {
    // SAFETY: the fixture server was started as a dedicated process group.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// Port of the #11721 exiting-parent-server regression @ 42849bb84.
#[cfg(unix)]
#[tokio::test]
async fn kills_children_holding_pipes_after_the_server_exited() {
    let mut options = StdioTransportOptions::new(fixture_command());
    options.args = vec!["--exiting-parent".to_owned()];
    options.close_timeout_ms = Some(100);
    let transport = StdioTransport::new(options);
    let close_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let close_count_callback = close_count.clone();
    let _subscription = transport.events().on_close(std::sync::Arc::new(move || {
        close_count_callback.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    transport.start().await.unwrap();
    let mut server_pid = None;
    let mut grandchild = None;
    for _ in 0..200 {
        let stderr = transport.stderr();
        server_pid = stderr.lines().find_map(|line| {
            line.strip_prefix("server ")
                .and_then(|value| value.trim().parse().ok())
        });
        grandchild = stderr.lines().find_map(|line| {
            line.strip_prefix("grandchild ")
                .and_then(|value| value.trim().parse().ok())
        });
        if server_pid.is_some() && grandchild.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let server_pid = server_pid.expect("server pid in stderr");
    let grandchild = grandchild.expect("grandchild pid in stderr");
    transport.close().await.unwrap();
    for _ in 0..100 {
        if !is_running(grandchild) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if is_running(grandchild) {
        kill_group(server_pid);
    }
    assert!(!is_running(grandchild), "grandchild {grandchild} survived");
    assert_eq!(close_count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn drop_kills_a_stubborn_process_group_without_an_explicit_close() {
    let mut options = StdioTransportOptions::new(fixture_command());
    options.args = vec!["--stubborn".to_owned()];
    let transport = StdioTransport::new(options);
    transport.start().await.unwrap();
    let server_pid = transport.pid().unwrap() as i32;
    let mut grandchild = None;
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
    drop(transport);
    for _ in 0..100 {
        if !is_running(server_pid) && !is_running(grandchild) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let leaked = is_running(server_pid) || is_running(grandchild);
    kill_group(server_pid);
    assert!(!leaked, "dropped transport left its process group alive");
}

#[cfg(unix)]
#[tokio::test]
async fn host_exit_kills_the_tracked_process_group() {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-process-guard-host"));
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    use tokio::io::AsyncBufReadExt;

    let mut host = command.spawn().unwrap();
    let mut lines = tokio::io::BufReader::new(host.stderr.take().unwrap()).lines();
    let mut server_pid = None;
    let mut grandchild = None;
    let mut captured = Vec::new();
    let capture = async {
        while server_pid.is_none() || grandchild.is_none() {
            let Ok(Some(line)) = lines.next_line().await else {
                break;
            };
            if let Some(pid) = line.strip_prefix("server ") {
                server_pid = pid.trim().parse().ok();
            }
            if let Some(pid) = line.strip_prefix("grandchild ") {
                grandchild = pid.trim().parse().ok();
            }
            captured.push(line);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), capture).await;
    let status = match tokio::time::timeout(Duration::from_secs(5), host.wait()).await {
        Ok(result) => Some(result.unwrap()),
        Err(_) => {
            let _ = host.kill().await;
            None
        }
    };
    let stderr = captured.join("\n");
    let (Some(server_pid), Some(grandchild)) = (server_pid, grandchild) else {
        panic!("fixture did not report its processes: {stderr}");
    };
    for _ in 0..100 {
        if !is_running(server_pid) && !is_running(grandchild) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let leaked = is_running(server_pid) || is_running(grandchild);
    kill_group(server_pid);
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Ok(Some(line)) = lines.next_line().await {
            captured.push(line);
        }
    })
    .await;
    let stderr = captured.join("\n");
    assert!(status.is_some_and(|status| status.success()), "{stderr}");
    assert!(!leaked, "host exited with live server processes: {stderr}");
}
