//! Integration tests for the bash tool and bash-executor.
//!
//! Ports of `test/tools.test.ts` bash tests (§15.1) and bash-executor tests.
//! These spawn real child processes (echo, sleep, printf) — no network.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir::tools::bash::{
    create_bash_tool, create_local_bash_operations, BashExecOptions, BashSpawnContext,
    BashToolOptions,
};
use pir::tools::bash_executor::{execute_bash, BashExecutorOptions};
use pir::tools::{SessionEnv, ToolContext};
use pir_agent::types::AgentTool;
use tokio_util::sync::CancellationToken;

fn test_ctx() -> ToolContext {
    ToolContext {
        cwd: PathBuf::from("."),
        session_env: None,
    }
}

/// Helper: run a bash tool command and return the text output or error message.
async fn run_bash(
    tool: &Arc<dyn AgentTool>,
    command: &str,
    timeout: Option<f64>,
) -> Result<String, String> {
    let mut params = serde_json::json!({ "command": command });
    if let Some(t) = timeout {
        params["timeout"] = serde_json::json!(t);
    }
    match tool
        .execute("test-call", params, CancellationToken::new(), None)
        .await
    {
        Ok(result) => {
            let text = result
                .content
                .iter()
                .map(|c| match c {
                    pir_ai::types::ToolResultContent::Text(t) => t.text.clone(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(text)
        }
        Err(e) => Err(format!("{e}")),
    }
}

mod bash_tool_tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_echo() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let out = run_bash(&tool, "echo 'test output'", None).await;
        assert!(out.unwrap().contains("test output"));
    }

    #[tokio::test]
    async fn test_exit_1_error() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let err = run_bash(&tool, "exit 1", None).await.unwrap_err();
        assert!(err.contains("Command exited with code 1"), "got: {err}");
    }

    /// stdout and stderr are merged into a single output stream (bash.ts:124-125).
    #[tokio::test]
    async fn test_stdout_stderr_merged() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let out = run_bash(&tool, "echo out-line; echo err-line >&2", None)
            .await
            .unwrap();
        assert!(out.contains("out-line"), "missing stdout: {out}");
        assert!(out.contains("err-line"), "missing stderr: {out}");
    }

    /// Timeout validation (bash.ts:27-38): no default timeout; non-finite/<=0
    /// rejected; above 2^31-1 ms rejected with the exact maximum in the message.
    #[tokio::test]
    async fn test_invalid_timeout_validation() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let err = run_bash(&tool, "true", Some(-1.0)).await.unwrap_err();
        assert!(
            err.contains("Invalid timeout: must be a finite number of seconds"),
            "got: {err}"
        );
        let err = run_bash(&tool, "true", Some(2_147_484.0))
            .await
            .unwrap_err();
        assert!(
            err.contains("Invalid timeout: maximum is 2147483.647 seconds"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_timeout() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let err = run_bash(&tool, "sleep 5", Some(1.0)).await.unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn test_cwd_does_not_exist() {
        let ctx = ToolContext {
            cwd: PathBuf::from("/nonexistent/path/xyz123"),
            session_env: None,
        };
        let tool = create_bash_tool(&ctx, BashToolOptions::default());
        let err = run_bash(&tool, "echo test", None).await.unwrap_err();
        assert!(
            err.contains("Working directory does not exist"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_spawn_error_bad_shell() {
        let tool = create_bash_tool(
            &test_ctx(),
            BashToolOptions {
                shell_path: Some("/nonexistent-shell-path-xyz123".into()),
                ..Default::default()
            },
        );
        let err = run_bash(&tool, "echo test", None).await.unwrap_err();
        assert!(err.contains("Custom shell path not found"), "got: {err}");
    }

    #[tokio::test]
    async fn test_command_prefix() {
        let tool = create_bash_tool(
            &test_ctx(),
            BashToolOptions {
                command_prefix: Some("echo PREFIX".into()),
                ..Default::default()
            },
        );
        let out = run_bash(&tool, "echo CMD", None).await.unwrap();
        assert!(out.contains("PREFIX"), "out: {out}");
        assert!(out.contains("CMD"), "out: {out}");
    }

    #[tokio::test]
    async fn test_no_prefix() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let out = run_bash(&tool, "echo hello", None).await.unwrap();
        assert!(out.contains("hello"));
    }

    #[tokio::test]
    async fn test_trailing_newline_not_extra_line() {
        // 4000 lines with trailing newline → totalLines should be 4000 not 4001.
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let params =
            serde_json::json!({ "command": "for i in $(seq 1 4000); do echo \"line $i\"; done" });
        let result = tool
            .execute("test", params, CancellationToken::new(), None)
            .await
            .unwrap();
        // Check details for totalLines.
        if let Some(truncation) = result.details.get("truncation") {
            let total = truncation["totalLines"].as_u64().unwrap();
            assert_eq!(total, 4000, "trailing newline should not add extra line");
        }
    }

    #[tokio::test]
    async fn test_cross_chunk_utf8() {
        // The streaming UTF-8 decoder must handle multi-byte chars that arrive
        // in separate chunks.
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let out = run_bash(&tool, "printf '\\xc3\\xa9'", None).await.unwrap();
        assert!(out.contains('\u{00E9}'), "out: {out}");
    }

    #[tokio::test]
    async fn test_line_truncation_writes_temp_file() {
        // Generate enough output to exceed line limit (2000 lines).
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let params =
            serde_json::json!({ "command": "for i in $(seq 1 5000); do echo \"line $i\"; done" });
        let result = tool
            .execute("test", params, CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(
            result.details.get("truncation").is_some(),
            "should be truncated"
        );
        let full_path = result
            .details
            .get("fullOutputPath")
            .and_then(|v| v.as_str());
        assert!(full_path.is_some(), "should have full output path");
        if let Some(path) = full_path {
            assert!(
                std::path::Path::new(path).exists(),
                "temp file should exist"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn test_truncated_timeout_includes_full_output() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let command = "for i in $(seq 1 5000); do echo \"line $i\"; done; sleep 5";
        let err = run_bash(&tool, command, Some(1.0)).await.unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
        assert!(err.contains("Full output:"), "should include path: {err}");
        // Clean up temp file if present.
        if let Some(idx) = err.find("Full output: ") {
            let path_str = err[idx + "Full output: ".len()..].trim_end_matches(']');
            let _ = std::fs::remove_file(path_str.trim());
        }
    }

    #[tokio::test]
    async fn test_spawn_hook_called() {
        let hook_called = Arc::new(Mutex::new(false));
        let hook_called2 = Arc::clone(&hook_called);

        let tool = create_bash_tool(
            &test_ctx(),
            BashToolOptions {
                spawn_hook: Some(Arc::new(move |ctx: BashSpawnContext| {
                    *hook_called2.lock().unwrap() = true;
                    // Inject a marker env var so we can verify it in the child.
                    let mut env = ctx.env;
                    env.insert("PIR_TEST_HOOK".into(), "yes".into());
                    // Change the command to verify rewrite.
                    BashSpawnContext {
                        command: "echo hooked".into(),
                        cwd: ctx.cwd,
                        env,
                    }
                })),
                ..Default::default()
            },
        );

        let out = run_bash(&tool, "echo original", None).await.unwrap();
        assert!(*hook_called.lock().unwrap(), "spawn hook should be called");
        assert!(out.contains("hooked"), "hooked command output: {out}");
    }

    #[tokio::test]
    async fn test_spawn_hook_can_modify_env() {
        let tool = create_bash_tool(
            &test_ctx(),
            BashToolOptions {
                spawn_hook: Some(Arc::new(|ctx: BashSpawnContext| {
                    let mut env = ctx.env;
                    env.insert("MY_TEST_VAR".into(), "hello123".into());
                    BashSpawnContext {
                        command: "echo $MY_TEST_VAR".into(),
                        cwd: ctx.cwd,
                        env,
                    }
                })),
                ..Default::default()
            },
        );
        let out = run_bash(&tool, "echo noop", None).await.unwrap();
        assert!(out.contains("hello123"), "env var injected: {out}");
    }

    #[tokio::test]
    async fn test_pir_session_env_injected() {
        let ctx = ToolContext {
            cwd: PathBuf::from("."),
            session_env: Some(SessionEnv {
                session_id: "test-session-123".into(),
                session_file: Some(PathBuf::from("/tmp/test.jsonl")),
                provider: Some("test-provider".into()),
                model: Some("test-model".into()),
                reasoning_level: Some("high".into()),
            }),
        };
        let tool = create_bash_tool(&ctx, BashToolOptions::default());
        let out = run_bash(&tool, "env", None).await.unwrap();
        assert!(
            out.contains("PIR_SESSION_ID=test-session-123"),
            "out: {out}"
        );
        assert!(
            out.contains("PIR_SESSION_FILE=/tmp/test.jsonl"),
            "out: {out}"
        );
        assert!(out.contains("PIR_PROVIDER=test-provider"), "out: {out}");
        assert!(out.contains("PIR_MODEL=test-model"), "out: {out}");
        assert!(out.contains("PIR_REASONING_LEVEL=high"), "out: {out}");
    }

    #[tokio::test]
    async fn test_pir_env_stripped_when_no_session() {
        // Set a PIR_ var in the process env — the tool should strip it.
        std::env::set_var("PIR_SESSION_ID", "should_be_stripped");
        std::env::set_var("PIR_MODEL", "should_be_stripped_too");

        let ctx = ToolContext {
            cwd: PathBuf::from("."),
            session_env: None,
        };
        let tool = create_bash_tool(&ctx, BashToolOptions::default());
        let out = run_bash(&tool, "env", None).await.unwrap();
        assert!(
            !out.contains("should_be_stripped"),
            "PIR_ vars should be stripped: {out}"
        );

        std::env::remove_var("PIR_SESSION_ID");
        std::env::remove_var("PIR_MODEL");
    }

    #[tokio::test]
    async fn test_expose_session_env_false() {
        let ctx = ToolContext {
            cwd: PathBuf::from("."),
            session_env: Some(SessionEnv {
                session_id: "secret-session".into(),
                session_file: None,
                provider: None,
                model: None,
                reasoning_level: None,
            }),
        };
        let tool = create_bash_tool(
            &ctx,
            BashToolOptions {
                expose_session_environment: false,
                ..Default::default()
            },
        );
        let out = run_bash(&tool, "env", None).await.unwrap();
        assert!(
            !out.contains("secret-session"),
            "session env should not be exposed: {out}"
        );
    }

    #[tokio::test]
    async fn test_coalesce_chatty_output() {
        // 5000 lines of output, count onUpdate calls — should be < 25 due to
        // throttling.
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());

        let update_count = Arc::new(Mutex::new(0u32));
        let uc2 = Arc::clone(&update_count);

        let callback = Box::new(move |_result| {
            *uc2.lock().unwrap() += 1;
        });

        let params =
            serde_json::json!({ "command": "for i in $(seq 1 5000); do echo \"line $i\"; done" });
        let _ = tool
            .execute("test", params, CancellationToken::new(), Some(callback))
            .await;

        let count = *update_count.lock().unwrap();
        assert!(
            count < 25,
            "update count should be < 25 due to throttling, got {count}"
        );
    }

    #[tokio::test]
    async fn test_aborted_command() {
        let tool = create_bash_tool(&test_ctx(), BashToolOptions::default());
        let token = CancellationToken::new();
        let token2 = token.clone();

        // Cancel after a short delay.
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            token2.cancel();
        });

        let params = serde_json::json!({ "command": "sleep 30" });
        let result = tool.execute("test", params, token, None).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("aborted"), "got: {err}");
    }

    #[tokio::test]
    async fn test_create_local_bash_operations_exposed() {
        // Verify the function is callable and returns a working backend.
        let ops = create_local_bash_operations(None);
        let collected: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let c2 = Arc::clone(&collected);
        let on_data = move |data: Vec<u8>| {
            c2.lock().unwrap().extend_from_slice(&data);
        };
        let result = ops
            .exec(
                "echo hello",
                std::path::Path::new("."),
                BashExecOptions {
                    signal: CancellationToken::new(),
                    timeout: None,
                    env: None,
                },
                &on_data,
            )
            .await
            .unwrap();
        assert_eq!(result, Some(0));
        let binding = collected.lock().unwrap();
        let output = String::from_utf8_lossy(&binding);
        assert!(output.contains("hello"));
    }
}

mod bash_executor_tests {
    use super::*;

    #[tokio::test]
    async fn test_ansi_stripped() {
        let ops = create_local_bash_operations(None);
        let result = execute_bash(
            "printf '\\033[31mred\\033[0m'",
            std::path::Path::new("."),
            ops.as_ref(),
            BashExecutorOptions {
                on_chunk: None,
                signal: CancellationToken::new(),
            },
        )
        .await
        .unwrap();

        assert!(result.output.contains("red"), "out: {}", result.output);
        assert!(
            !result.output.contains("\x1b[31m"),
            "ANSI should be stripped: {:?}",
            result.output
        );
    }

    #[tokio::test]
    async fn test_carriage_return_removed() {
        let ops = create_local_bash_operations(None);
        let result = execute_bash(
            "printf 'hello\\r\\nworld'",
            std::path::Path::new("."),
            ops.as_ref(),
            BashExecutorOptions {
                on_chunk: None,
                signal: CancellationToken::new(),
            },
        )
        .await
        .unwrap();

        assert!(
            !result.output.contains('\r'),
            "\\r should be removed: {:?}",
            result.output
        );
        assert!(result.output.contains("hello"), "out: {}", result.output);
        assert!(result.output.contains("world"), "out: {}", result.output);
    }

    #[tokio::test]
    async fn test_large_output_writes_temp_file() {
        // Generate > 50KB of output.
        let ops = create_local_bash_operations(None);
        let result = execute_bash(
            "for i in $(seq 1 6000); do echo \"padding line $i with extra text to fill bytes\"; done",
            std::path::Path::new("."),
            ops.as_ref(),
            BashExecutorOptions {
                on_chunk: None,
                signal: CancellationToken::new(),
            },
        )
        .await
        .unwrap();

        assert!(result.truncated, "output should be truncated");
        assert!(
            result.full_output_path.is_some(),
            "temp file should be created"
        );
        if let Some(ref path) = result.full_output_path {
            assert!(path.exists(), "temp file should exist");
            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn test_abort_returns_cancelled() {
        let ops = create_local_bash_operations(None);
        let token = CancellationToken::new();
        let token2 = token.clone();

        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            token2.cancel();
        });

        let result = execute_bash(
            "sleep 30",
            std::path::Path::new("."),
            ops.as_ref(),
            BashExecutorOptions {
                on_chunk: None,
                signal: token,
            },
        )
        .await
        .unwrap();

        assert!(result.cancelled, "should be cancelled");
        assert!(result.exit_code.is_none(), "exit code should be None");
    }

    #[tokio::test]
    async fn test_simple_command() {
        let ops = create_local_bash_operations(None);
        let result = execute_bash(
            "echo test123",
            std::path::Path::new("."),
            ops.as_ref(),
            BashExecutorOptions {
                on_chunk: None,
                signal: CancellationToken::new(),
            },
        )
        .await
        .unwrap();

        assert!(result.output.contains("test123"));
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.cancelled);
        assert!(!result.truncated);
    }
}

mod process_group_tests {
    use super::*;

    #[tokio::test]
    async fn test_no_zombie_after_cancel() {
        // Cancel a sleep command and verify the process group is cleaned up.
        let ops = create_local_bash_operations(None);
        let token = CancellationToken::new();
        let token2 = token.clone();

        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
            token2.cancel();
        });

        let _ = ops
            .exec(
                "sleep 30",
                std::path::Path::new("."),
                BashExecOptions {
                    signal: token,
                    timeout: None,
                    env: None,
                },
                &|_data: Vec<u8>| {},
            )
            .await;

        // Give a moment for signal propagation.
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Check that no "sleep 30" processes remain. The bracket pattern
        // `[s]leep 30` avoids matching this probe's own `sh -c` command line,
        // which literally contains the search string.
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '[s]leep 30' || true")
            .output()
            .unwrap();
        let remaining = String::from_utf8_lossy(&output.stdout);
        let count = remaining.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(
            count, 0,
            "no sleep 30 processes should remain, found: {remaining}"
        );
    }
}
