//! Test-only fixed child process for the subagents e2e suite.
//!
//! Stands in for `rpi --mode json -p`: dumps argv + RPI_SUBAGENT_* env to
//! `$RPI_E2E_DUMP_DIR`, then emits the event stream selected by
//! `$RPI_E2E_MODE` and exits. Not part of the distributed plugin — a `bin`
//! target so integration tests get `CARGO_BIN_EXE_rpi-subagents-fixed-child`.

use std::io::Write;

fn main() {
    let mode = std::env::var("RPI_E2E_MODE").unwrap_or_else(|_| "ok".to_string());
    if let Ok(dump_dir) = std::env::var("RPI_E2E_DUMP_DIR") {
        let _ = std::fs::create_dir_all(&dump_dir);
        let args: Vec<String> = std::env::args().collect();
        let _ = std::fs::write(
            std::path::Path::new(&dump_dir).join("argv.txt"),
            args.join("\n"),
        );
        let mut env_pairs: Vec<(String, String)> = std::env::vars()
            .filter(|(key, _)| key.starts_with("RPI_") || key.starts_with("MCP") || key == "PATH")
            .collect();
        env_pairs.sort();
        let _ = std::fs::write(
            std::path::Path::new(&dump_dir).join("env.txt"),
            env_pairs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let _ = std::fs::write(
            std::path::Path::new(&dump_dir).join("pid.txt"),
            std::process::id().to_string(),
        );
        // Snapshot the system-prompt temp file before the parent cleans it up.
        let mut iter = args.iter().skip(1);
        while let Some(arg) = iter.next() {
            if arg == "--system-prompt" || arg == "--append-system-prompt" {
                if let Some(path) = iter.next() {
                    if let Ok(content) = std::fs::read_to_string(path) {
                        let _ = std::fs::write(
                            std::path::Path::new(&dump_dir).join("prompt.md"),
                            content,
                        );
                        #[cfg(unix)]
                        if let Ok(meta) = std::fs::metadata(path) {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::write(
                                std::path::Path::new(&dump_dir).join("prompt.mode"),
                                format!("{:o}", meta.permissions().mode() & 0o777),
                            );
                        }
                    }
                }
            }
        }
    }

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let emit = |out: &mut std::io::BufWriter<std::io::StdoutLock<'_>>, line: &str| {
        let _ = writeln!(out, "{line}");
    };
    match mode.as_str() {
        "ok" => {
            emit(
                &mut out,
                r#"{"type":"session","version":3,"id":"fixed","timestamp":"2026-08-14T00:00:00.000Z","cwd":"/tmp"}"#,
            );
            emit(
                &mut out,
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Fixed child result: analysis complete"}],"usage":{"input":100,"output":20,"cacheRead":5,"cacheWrite":3,"cost":{"total":0.42}},"model":"faux/fixed-1","stopReason":"stop"}}"#,
            );
            emit(&mut out, r#"{"type":"agent_settled"}"#);
        }
        "willretry" => {
            emit(&mut out, r#"{"type":"agent_end","willRetry":true}"#);
            emit(
                &mut out,
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"after retry"}],"stopReason":"stop"}}"#,
            );
            emit(&mut out, r#"{"type":"agent_settled"}"#);
        }
        "fail" => {
            emit(
                &mut out,
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"partial"}],"stopReason":"error","errorMessage":"model exploded"}}"#,
            );
            let _ = out.flush();
            std::process::exit(3);
        }
        "rawjunk" => {
            emit(&mut out, "this is not json at all");
            let _ = out.flush();
            std::process::exit(7);
        }
        "partial_then_hang" => {
            emit(
                &mut out,
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"partial output before hanging"}],"stopReason":"stop"}}"#,
            );
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
        "partial_toolcall_then_hang" => {
            // Not a terminal event (toolCall in content): only the timeout
            // ladder can reclaim this child.
            emit(
                &mut out,
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"read","arguments":{}},{"type":"text","text":"partial output before hanging"}],"stopReason":"stop"}}"#,
            );
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
        "hang" => {
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
        other => {
            eprintln!("fixed-child: unknown mode {other}");
            std::process::exit(64);
        }
    }
    let _ = out.flush();
}
