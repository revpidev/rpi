//! Test-only host that exits without dropping an active stdio transport.

#[cfg(unix)]
#[tokio::main]
async fn main() {
    use rpi_mcp::transport::stdio::{StderrMode, StdioTransportOptions};
    use rpi_mcp::{McpTransport, StdioTransport};

    let server = std::env::current_exe()
        .expect("host executable")
        .with_file_name("mcp-test-server");
    let mut options = StdioTransportOptions::new(server.to_string_lossy().into_owned());
    options.args = vec!["--parent".to_owned()];
    options.stderr = StderrMode::Inherit;
    let transport = StdioTransport::new(options);
    transport.start().await.expect("start MCP server");
    eprintln!("server {}", transport.pid().expect("server pid"));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    std::process::exit(0);
}

#[cfg(not(unix))]
fn main() {}
