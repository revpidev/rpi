//! Test-only stdio MCP server used by `tests/stdio.rs` (counterpart of
//! `packages/mcp/test/fixtures/stdio-server.mjs` @ a13d35a74).
//!
//! Modes:
//! - default: LF-delimited JSON-RPC echo server; exits on stdin EOF.
//! - `--stubborn`: ignores SIGTERM and stdin EOF, spawns a grandchild (itself
//!   with `--grandchild`) that keeps running, so shutdown must reap the whole
//!   process group.
//! - `--grandchild`: sleeps forever.

use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--grandchild") {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    let stubborn = args.iter().any(|arg| arg == "--stubborn");
    #[cfg(unix)]
    if stubborn {
        // SAFETY: the process is single-threaded at this point and the
        // handler only ignores SIGTERM (async-signal-safe).
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    if stubborn {
        // The grandchild must outlive this server; the transport's process
        // group kill reaps it on shutdown, so it is intentionally not waited.
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new(std::env::current_exe().expect("current exe"))
            .arg("--grandchild")
            .spawn()
            .expect("spawn grandchild");
        eprintln!("grandchild {}", child.id());
    }
    eprintln!("stdio fixture ready");
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if stubborn {
                    // Ignore stdin EOF: only a signal can stop this server.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                return;
            }
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(id) = message.get("id") else {
            // Notification.
            continue;
        };
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "stdio-fixture", "version": "1.0.0"},
            }),
            "tools/list" => serde_json::json!({
                "tools": [{"name": "echo", "inputSchema": {"type": "object"}}],
            }),
            "tools/call" => {
                let text = message
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .and_then(|args| args.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                serde_json::json!({"content": [{"type": "text", "text": text}]})
            }
            _ => {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("Method not found: {method}")},
                });
                println!("{response}");
                let _ = std::io::stdout().flush();
                continue;
            }
        };
        let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
        println!("{response}");
        let _ = std::io::stdout().flush();
    }
}
