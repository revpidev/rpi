//! Durable tool integration tests (port of `packages/durable/test/tools.test.ts`
//! @ `4a080252f` and the environment-facing cases of `harness-tools`).

mod generation_support;
#[path = "tools_support.rs"]
mod tools_support;

use std::sync::Arc;

use rpi_durable::env::node::{NodeExecutionEnv, NodeExecutionEnvOptions};
use rpi_durable::env::types::{ExecutionEnv, FileContent};
use rpi_durable::harness::types::ToolRegistration;
use rpi_durable::tools::{
    create_bash_tool, create_edit_tool, create_power_shell_tool, create_read_tool,
    create_write_tool, prepare_edit_arguments,
};
use serde_json::json;
use tools_support::{FakeApi, TempDir, diagnostic_text, run_tool, run_tool_with_api, text_output};

fn create_env(dir: &TempDir) -> Arc<dyn ExecutionEnv> {
    Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: Some(dir.path_str()),
        ..Default::default()
    }))
}

async fn write_file(env: &Arc<dyn ExecutionEnv>, path: &str, content: &str) {
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    env.write_file(path, FileContent::from(content), &context)
        .await
        .expect("write");
}

async fn read_file(env: &Arc<dyn ExecutionEnv>, path: &str) -> String {
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    env.read_text_file(path, &context).await.expect("read")
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_fail_without_an_environment() {
    let (result, _, _) = run_tool(&create_read_tool(), json!({ "path": "x" }), None).await;
    let error = result.expect_err("must fail");
    assert!(
        error.to_string().contains("No execution environment"),
        "got {error}"
    );

    let (result, _, _) = run_tool(
        &create_write_tool(),
        json!({ "path": "x", "content": "y" }),
        None,
    )
    .await;
    assert!(result.is_err(), "write must fail without an environment");
    let (result, _, _) = run_tool(
        &create_edit_tool(),
        json!({ "path": "x", "edits": [] }),
        None,
    )
    .await;
    assert!(result.is_err(), "edit must fail without an environment");
    let (result, _, _) = run_tool(
        &create_bash_tool(None),
        json!({ "command": "echo x" }),
        None,
    )
    .await;
    assert!(result.is_err(), "bash must fail without an environment");
}

#[tokio::test]
async fn read_reports_offsets_and_limits_as_diagnostics() {
    let dir = TempDir::new("durable-read-offset");
    let env = create_env(&dir);
    let content = (1..=100)
        .map(|index| format!("Line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&env, "test.txt", &content).await;

    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "test.txt", "offset": 41, "limit": 20 }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    let output = text_output(&result);
    assert!(!output.contains("Line 40"));
    assert!(output.contains("Line 41"));
    assert!(output.contains("Line 60"));
    assert!(!output.contains("Line 61"));
    assert_eq!(
        diagnostic_text(&result),
        "40 more lines in file. Use offset=61 to continue."
    );
}

#[tokio::test]
async fn read_truncates_large_text_by_line_count() {
    let dir = TempDir::new("durable-read-large");
    let env = create_env(&dir);
    let content = (1..=2500)
        .map(|index| format!("Line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&env, "large.txt", &content).await;

    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "large.txt" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    assert_eq!(
        diagnostic_text(&result),
        "Showing lines 1-2000 of 2500. Use offset=2001 to continue."
    );
    assert_eq!(
        result
            .diagnostics
            .as_ref()
            .and_then(|diagnostics| diagnostics.first())
            .and_then(|diagnostic| diagnostic.code.as_deref()),
        Some("truncated")
    );
    let truncation = result
        .details
        .as_ref()
        .and_then(|details| details.get("truncation"))
        .expect("truncation details");
    assert_eq!(truncation.get("truncated"), Some(&json!(true)));
    assert_eq!(truncation.get("truncatedBy"), Some(&json!("lines")));
    assert_eq!(truncation.get("totalLines"), Some(&json!(2500)));
    assert_eq!(truncation.get("outputLines"), Some(&json!(2000)));
}

#[tokio::test]
async fn read_does_not_count_a_trailing_newline_as_an_extra_line() {
    let dir = TempDir::new("durable-read-exact");
    let env = create_env(&dir);
    let content = format!("{}\n", vec!["x"; 2000].join("\n"));
    write_file(&env, "exact.txt", &content).await;

    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "exact.txt" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    assert!(result.details.is_none());
    assert_eq!(result.diagnostics, Some(Vec::new()));
}

#[tokio::test]
async fn read_shows_the_start_of_a_line_over_the_byte_limit() {
    let dir = TempDir::new("durable-read-long");
    let env = create_env(&dir);
    write_file(&env, "long.txt", &format!("{}\nnext\n", "é".repeat(40_000))).await;

    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "long.txt" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    assert_eq!(text_output(&result), "é".repeat(25_600));
    assert_eq!(
        diagnostic_text(&result),
        "Line 1 is 78.1KB, exceeds the 50.0KB limit; showing its first 50.0KB. Use bash: sed -n '1p' long.txt | tail -c +51201"
    );
    let truncation = result
        .details
        .as_ref()
        .and_then(|details| details.get("truncation"))
        .expect("truncation details");
    assert_eq!(truncation.get("truncated"), Some(&json!(true)));
    assert_eq!(truncation.get("firstLineExceedsLimit"), Some(&json!(true)));
    assert_eq!(truncation.get("outputBytes"), Some(&json!(51_200)));
    assert_eq!(truncation.get("outputLines"), Some(&json!(1)));
    assert!(truncation.get("content").is_none());
}

#[tokio::test]
async fn read_rejects_offsets_beyond_the_file() {
    let dir = TempDir::new("durable-read-beyond");
    let env = create_env(&dir);
    write_file(&env, "short.txt", "one\ntwo\nthree").await;
    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "short.txt", "offset": 100 }),
        Some(Arc::clone(&env)),
    )
    .await;
    let error = result.expect_err("must fail");
    assert_eq!(
        error.to_string(),
        "Offset 100 is beyond end of file (3 lines total)"
    );
}

#[tokio::test]
async fn read_returns_images_detected_by_content() {
    let dir = TempDir::new("durable-read-image");
    let env = create_env(&dir);
    let png = base64_decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==",
    );
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    env.write_file("image.txt", FileContent::Bytes(png), &context)
        .await
        .expect("write image");
    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "image.txt" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    assert!(diagnostic_text(&result).contains("Read image file [image/png]"));
    let content = result.output.expect("content");
    assert!(matches!(
        content.first(),
        Some(rpi_ai::types::ToolResultContent::Image(image))
            if image.mime_type == "image/png"
    ));
}

fn base64_decode(input: &str) -> Vec<u8> {
    // Minimal standard-alphabet decoder for the fixed PNG fixture.
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = value(byte) else {
            continue;
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_writes_files_and_creates_parent_directories() {
    let dir = TempDir::new("durable-write");
    let env = create_env(&dir);
    let (result, _, _) = run_tool(
        &create_write_tool(),
        json!({ "path": "nested/dir/file.txt", "content": "hello" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("write");
    assert_eq!(
        text_output(&result),
        "Successfully wrote to nested/dir/file.txt"
    );
    assert_eq!(read_file(&env, "nested/dir/file.txt").await, "hello");
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_applies_disjoint_edits_and_returns_both_diff_formats() {
    let dir = TempDir::new("durable-edit");
    let env = create_env(&dir);
    write_file(&env, "edit.txt", "alpha\nbeta\ngamma\ndelta\n").await;
    let (result, _, _) = run_tool(
        &create_edit_tool(),
        json!({
            "path": "edit.txt",
            "edits": [
                { "oldText": "alpha\n", "newText": "ALPHA\n" },
                { "oldText": "gamma\n", "newText": "GAMMA\n" }
            ]
        }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("edit");
    assert_eq!(
        text_output(&result),
        "Successfully replaced 2 block(s) in edit.txt."
    );
    let details = result.details.expect("details");
    let diff = details
        .get("diff")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let patch = details
        .get("patch")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(diff.contains("ALPHA"), "got {diff}");
    assert!(diff.contains("GAMMA"), "got {diff}");
    assert!(
        patch.starts_with("--- edit.txt\n+++ edit.txt\n"),
        "got {patch}"
    );
    assert!(patch.contains("@@"), "got {patch}");
    assert_eq!(
        read_file(&env, "edit.txt").await,
        "ALPHA\nbeta\nGAMMA\ndelta\n"
    );
}

#[tokio::test]
async fn edit_repairs_edits_sent_as_a_json_string_or_single_object() {
    let edit = json!({ "oldText": "a", "newText": "b" });
    let as_string =
        json!({ "path": "f", "edits": serde_json::to_string(&json!([edit])) .expect("json") });
    let repaired = prepare_edit_arguments(&as_string).expect("repair");
    assert_eq!(repaired, json!({ "path": "f", "edits": [edit] }));
    // The original value is unchanged.
    assert!(
        as_string
            .get("edits")
            .and_then(|value| value.as_str())
            .is_some()
    );

    let single = json!({ "path": "f", "edits": serde_json::to_string(&edit).expect("json") });
    assert_eq!(
        prepare_edit_arguments(&single).expect("repair"),
        json!({ "path": "f", "edits": [edit] })
    );

    let legacy = json!({ "path": "f", "oldText": "a", "newText": "b" });
    assert_eq!(
        prepare_edit_arguments(&legacy).expect("repair"),
        json!({ "path": "f", "edits": [edit] })
    );
}

#[tokio::test]
async fn edit_reports_missing_and_duplicate_old_text() {
    let dir = TempDir::new("durable-edit-errors");
    let env = create_env(&dir);
    write_file(&env, "edit.txt", "alpha\nalpha\n").await;
    let (result, _, _) = run_tool(
        &create_edit_tool(),
        json!({ "path": "edit.txt", "edits": [{ "oldText": "missing", "newText": "x" }] }),
        Some(Arc::clone(&env)),
    )
    .await;
    let error = result.expect_err("must fail");
    assert!(
        error
            .to_string()
            .contains("Could not find the exact text in edit.txt."),
        "got {error}"
    );

    let (result, _, _) = run_tool(
        &create_edit_tool(),
        json!({ "path": "edit.txt", "edits": [{ "oldText": "alpha\n", "newText": "x" }] }),
        Some(Arc::clone(&env)),
    )
    .await;
    let error = result.expect_err("must fail");
    assert!(
        error
            .to_string()
            .contains("Found 2 occurrences of the text in edit.txt. The text must be unique."),
        "got {error}"
    );
}

// ---------------------------------------------------------------------------
// bash / powershell
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn shell_available() -> bool {
    std::path::Path::new("/bin/bash").exists() || std::path::Path::new("/bin/sh").exists()
}

#[cfg(unix)]
#[tokio::test]
async fn bash_runs_a_command_and_reports_output() {
    if !shell_available() {
        return;
    }
    let dir = TempDir::new("durable-bash");
    let env = create_env(&dir);
    let (result, output, _) = run_tool(
        &create_bash_tool(None),
        json!({ "command": "printf 'hello'" }),
        Some(Arc::clone(&env)),
    )
    .await;
    result.expect("bash");
    assert!(output.contains("hello"), "got {output}");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_reports_a_nonzero_exit_as_an_error_and_keeps_the_output() {
    if !shell_available() {
        return;
    }
    let dir = TempDir::new("durable-bash-exit");
    let env = create_env(&dir);
    let (result, output, _diagnostics) = run_tool(
        &create_bash_tool(None),
        json!({ "command": "printf 'out'; printf 'err' >&2; exit 3" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("nonzero exit is an error result, not a throw");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result
            .diagnostics
            .iter()
            .flatten()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("exit_code")
                && diagnostic.message == "Command exited with code 3"),
        "got {:?}",
        result.diagnostics
    );
    let structured = result.structured_output.expect("structured output");
    assert_eq!(structured["exitCode"], 3);
    assert!(
        structured["output"].as_str().unwrap_or_default() == output,
        "structured output repeats the retained output"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_distinguishes_spawn_failure_from_a_nonzero_exit() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("durable-bash-spawn");
    let shell = dir.path().join("not-executable");
    std::fs::write(&shell, "not a shell").expect("shell fixture");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o644))
        .expect("shell permissions");
    let env: Arc<dyn ExecutionEnv> = Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: Some(dir.path_str()),
        shell_path: Some(shell.to_string_lossy().into_owned()),
        ..Default::default()
    }));
    let (result, _, _) = run_tool(
        &create_bash_tool(None),
        json!({ "command": "exit 0" }),
        Some(env),
    )
    .await;
    let error = result.expect_err("a non-executable shell is a spawn failure");
    let execution = error
        .downcast::<rpi_durable::env::types::ExecutionError>()
        .expect("execution error");
    assert_eq!(
        execution.code,
        rpi_durable::env::types::ExecutionErrorCode::SpawnError
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_distinguishes_cancellation_from_a_nonzero_exit() {
    let dir = TempDir::new("durable-bash-cancel");
    let env = create_env(&dir);
    let tool = create_bash_tool(None);
    let api = FakeApi::new(Some(env));
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let context = rpi_durable::chord::context::with_abort_signal(
        token,
        &rpi_durable::chord::context::BACKGROUND_CONTEXT,
    );
    let result = tool
        .execute
        .execute(serde_json::Map::new(), &api, &context)
        .await;
    let error = result.expect_err("a cancelled command is not a process result");
    let execution = error
        .downcast::<rpi_durable::env::types::ExecutionError>()
        .expect("execution error");
    assert_eq!(
        execution.code,
        rpi_durable::env::types::ExecutionErrorCode::Aborted
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_spills_large_output_and_reports_the_file() {
    if !shell_available() {
        return;
    }
    let dir = TempDir::new("durable-bash-spill");
    let env = create_env(&dir);
    let command = "i=0; while [ $i -lt 3000 ]; do echo line-$i; i=$((i+1)); done";
    let (result, _, diagnostics) = run_tool(
        &create_bash_tool(None),
        json!({ "command": command }),
        Some(Arc::clone(&env)),
    )
    .await;
    result.expect("bash");
    let spill = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some("full_output"))
        .expect("spill diagnostic");
    let path = spill.message.trim_start_matches("Full output: ");
    assert!(
        std::path::Path::new(path).is_file(),
        "spill file {path} must exist"
    );
    let content = std::fs::read_to_string(path).expect("spill content");
    assert!(
        content.contains("line-2999"),
        "spill must hold the complete output"
    );
}

/// The committed fake PowerShell, never written at test time (a
/// write-then-exec script is flaky on Linux: ETXTBSY when a concurrent fork
/// copies its open write fd).
#[cfg(unix)]
fn fake_pwsh() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-pwsh").to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn powershell_transports_its_argv_without_a_shell() {
    let dir = TempDir::new("durable-powershell");
    let env = create_env(&dir);
    let tool = create_power_shell_tool(Some(rpi_durable::tools::PowerShellToolOptions {
        programs: Some(vec![fake_pwsh()]),
        ..Default::default()
    }));
    let (result, output, _) = run_tool(
        &tool,
        json!({ "command": "Write-Output hi" }),
        Some(Arc::clone(&env)),
    )
    .await;
    result.expect("powershell");
    assert!(
        output.contains("-NoProfile|-NonInteractive|-ExecutionPolicy|Bypass|-Command|"),
        "got {output}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn powershell_falls_back_to_the_next_program() {
    let dir = TempDir::new("durable-powershell-fallback");
    let env = create_env(&dir);
    let tool = create_power_shell_tool(Some(rpi_durable::tools::PowerShellToolOptions {
        programs: Some(vec!["rpi-durable-missing-pwsh".to_string(), fake_pwsh()]),
        ..Default::default()
    }));
    let (result, _, _) = run_tool(&tool, json!({ "command": "x" }), Some(Arc::clone(&env))).await;
    result.expect("fallback");
}

// ---------------------------------------------------------------------------
// Parameter schemas (the typebox replacement face)
// ---------------------------------------------------------------------------

/// Every tool declares an object schema whose properties carry types and
/// descriptions, and the shared `rpi-ai` validation face enforces required
/// fields and coerces numeric strings.
#[test]
fn tool_parameters_validate_and_are_described() {
    let tools = [
        create_read_tool(),
        create_write_tool(),
        create_edit_tool(),
        create_bash_tool(None),
        create_power_shell_tool(None),
    ];
    for tool in &tools {
        let parameters = &tool.tool.parameters;
        assert_eq!(
            parameters.get("type"),
            Some(&json!("object")),
            "{}",
            tool.name()
        );
        let properties = parameters
            .get("properties")
            .and_then(|value| value.as_object())
            .unwrap_or_else(|| panic!("{} declares properties", tool.name()));
        for (name, schema) in properties {
            assert!(
                schema.get("type").is_some(),
                "{}::{name} declares a type",
                tool.name()
            );
            assert!(
                schema
                    .get("description")
                    .and_then(|value| value.as_str())
                    .is_some_and(|description| !description.is_empty()),
                "{}::{name} has a description",
                tool.name()
            );
        }
        assert!(
            parameters.get("required").is_some(),
            "{} declares required fields",
            tool.name()
        );
    }

    let read = create_read_tool();
    let call = |arguments: serde_json::Value| rpi_ai::types::ToolCall {
        id: "c".to_string(),
        name: "read".to_string(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    };
    assert!(
        rpi_ai::utils::validation::validate_tool_arguments(&read.tool, &call(json!({}))).is_err(),
        "a missing required path is rejected"
    );
    let validated = rpi_ai::utils::validation::validate_tool_arguments(
        &read.tool,
        &call(json!({ "path": "x", "offset": "3" })),
    )
    .expect("numeric strings coerce");
    assert_eq!(validated.get("offset"), Some(&json!(3)));
}

/// The fake api's `details` path is exercised through `edit`; keep a compile
/// check that the support type is used.
#[tokio::test]
async fn fake_api_collects_details() {
    let api = FakeApi::new(None);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    rpi_durable::harness::types::ToolExecutionApi::details(&api, json!({ "a": 1 }), &context)
        .await
        .expect("details");
    assert_eq!(
        api.details.lock().expect("details").clone(),
        Some(json!({ "a": 1 }))
    );
}

/// Keep `ToolRegistration` imported for the helper alias check.
#[allow(dead_code)]
fn _registration_is_public(tool: &ToolRegistration) -> &str {
    tool.name()
}

// ---------------------------------------------------------------------------
// read images (V18-06 FR-K)
// ---------------------------------------------------------------------------

fn tiny_png() -> Vec<u8> {
    // 1x1 transparent PNG.
    vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

#[tokio::test]
async fn read_returns_a_supported_image_within_the_limits() {
    let dir = TempDir::new("durable-read-image");
    let env = create_env(&dir);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    env.write_file("tiny.png", FileContent::Bytes(tiny_png()), &context)
        .await
        .expect("write image");
    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "tiny.png" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read image");
    let content = result.output.expect("content");
    assert_eq!(content.len(), 1);
    assert!(matches!(
        &content[0],
        rpi_ai::types::ToolResultContent::Image(image)
            if image.mime_type == "image/png" && !image.data.is_empty()
    ));
    assert!(result.diagnostics.iter().flatten().any(|diagnostic| {
        diagnostic.code.as_deref() == Some("image")
            && diagnostic.message.contains("Read image file [")
            && diagnostic.message.contains("image/png")
    }));
}

#[tokio::test]
async fn read_rejects_an_image_over_the_pixel_limit() {
    let dir = TempDir::new("durable-read-image-large");
    let env = create_env(&dir);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    let mut header = tiny_png();
    // IHDR width/height: 3000x3000.
    header[16..20].copy_from_slice(&3000u32.to_be_bytes());
    header[20..24].copy_from_slice(&3000u32.to_be_bytes());
    env.write_file("large.png", FileContent::Bytes(header), &context)
        .await
        .expect("write image");
    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "large.png" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read image");
    assert_eq!(result.is_error, Some(true));
    assert!(result.diagnostics.iter().flatten().any(|diagnostic| {
        diagnostic.code.as_deref() == Some("unsupported_image")
            && diagnostic.message.contains("3000x3000")
    }));
}

// ---------------------------------------------------------------------------
// read image processor (V18-06 FR-K)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_with_a_processor_converts_bmp_to_png() {
    use rpi_durable::tools::image_processor::BuiltinImageProcessor;
    use rpi_durable::tools::read::{ReadToolOptions, create_read_tool_with_options};
    let dir = TempDir::new("durable-read-bmp");
    let env = create_env(&dir);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
    let mut bmp = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
        .expect("bmp");
    env.write_file("tiny.bmp", FileContent::Bytes(bmp), &context)
        .await
        .expect("write");
    let tool = create_read_tool_with_options(ReadToolOptions {
        image_processor: Some(Arc::new(BuiltinImageProcessor)),
    });
    let (result, _, _) =
        run_tool(&tool, json!({ "path": "tiny.bmp" }), Some(Arc::clone(&env))).await;
    let result = result.expect("read");
    let content = result.output.expect("content");
    assert!(matches!(
        content.first(),
        Some(rpi_ai::types::ToolResultContent::Image(image)) if image.mime_type == "image/png"
    ));
    assert!(result.diagnostics.iter().flatten().any(|diagnostic| {
        diagnostic.code.as_deref() == Some("image")
            && diagnostic.message.contains("Converted from image/bmp")
    }));
}

#[tokio::test]
async fn read_with_a_processor_resizes_an_image_over_the_pixel_limit() {
    use rpi_durable::tools::image_processor::BuiltinImageProcessor;
    use rpi_durable::tools::read::{ReadToolOptions, create_read_tool_with_options};
    let dir = TempDir::new("durable-read-resize");
    let env = create_env(&dir);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    let image = image::RgbaImage::from_pixel(3000, 100, image::Rgba([0, 0, 255, 255]));
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    env.write_file("large.png", FileContent::Bytes(png), &context)
        .await
        .expect("write");
    let tool = create_read_tool_with_options(ReadToolOptions {
        image_processor: Some(Arc::new(BuiltinImageProcessor)),
    });
    let (result, _, _) = run_tool(
        &tool,
        json!({ "path": "large.png" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    assert_eq!(result.is_error, None);
    assert!(result.diagnostics.iter().flatten().any(|diagnostic| {
        diagnostic.code.as_deref() == Some("image")
            && diagnostic.message.contains("Resized from 3000x100")
    }));
}

#[tokio::test]
async fn read_without_a_processor_errors_for_bmp() {
    use rpi_durable::tools::image_processor::BuiltinImageProcessor;
    let dir = TempDir::new("durable-read-bmp-error");
    let env = create_env(&dir);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
    let mut bmp = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
        .expect("bmp");
    env.write_file("tiny.bmp", FileContent::Bytes(bmp), &context)
        .await
        .expect("write");
    let (result, _, _) = run_tool(
        &create_read_tool(),
        json!({ "path": "tiny.bmp" }),
        Some(Arc::clone(&env)),
    )
    .await;
    let result = result.expect("read");
    assert_eq!(result.is_error, Some(true));
    assert!(result.diagnostics.iter().flatten().any(|diagnostic| {
        diagnostic.code.as_deref() == Some("unsupported_image")
            && diagnostic.message.contains("needs converting")
    }));
    let _ = BuiltinImageProcessor;
}

/// R-D10/read images: a model without image input gets the placeholder note
/// while the image remains visible to the transcript.
#[tokio::test]
async fn read_notes_a_model_without_vision() {
    let dir = TempDir::new("durable-read-no-vision");
    let env = create_env(&dir);
    let context = rpi_durable::chord::context::BACKGROUND_CONTEXT.clone();
    env.write_file("tiny.png", FileContent::Bytes(tiny_png()), &context)
        .await
        .expect("write image");
    let model = generation_support::model();
    let mut api = FakeApi::new(Some(Arc::clone(&env)));
    api.model = Some(rpi_durable::harness::ModelRef {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
    });
    api.models = Some(generation_support::test_models(Vec::new()));
    let (result, _, _) =
        run_tool_with_api(&create_read_tool(), json!({ "path": "tiny.png" }), api).await;
    let result = result.expect("read image");
    assert!(matches!(
        result.output.as_deref(),
        Some([rpi_ai::types::ToolResultContent::Image(_)])
    ));
    assert!(result.diagnostics.iter().flatten().any(|diagnostic| {
        diagnostic.code.as_deref() == Some("image")
            && diagnostic.message.contains("does not support images")
    }));
}
