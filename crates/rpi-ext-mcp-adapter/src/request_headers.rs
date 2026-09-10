//! Per-request HTTP header derivation from a trusted command
//! (R7.2.11.2 / #353, `request-headers-command.ts` @ 10a45367).
//!
//! `requestHeadersCommand` on a server entry derives request headers from
//! the EXACT outbound request on every Streamable HTTP / SSE call: the
//! command receives `{version: 1, method, url, bodyBase64}` on stdin and
//! must answer a JSON object of string header values on stdout (≤64 KiB,
//! within `timeoutMs`, default 10s). The contract is fail-closed: any
//! failure (spawn, non-zero exit, invalid JSON, non-string value, invalid
//! header, timeout, oversized output) fails the REQUEST — the request is
//! never sent without the derived headers.
//!
//! Upstream's POSIX `ps axeww` descendant-stabilization passes are Node
//! cleanup hardening; the Rust port reaps via the stdio.rs precedent —
//! dedicated process group + `kill_on_drop` + explicit group SIGKILL on
//! timeout/oversize (G4 no-leftover-process red line).
//!
//! Security (G4): the envelope carries the request BODY (which may include
//! tool arguments) but no credentials; derived header VALUES must never be
//! logged.

use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// `DEFAULT_TIMEOUT_MS` (request-headers-command.ts:9).
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// `MAX_OUTPUT_BYTES` (request-headers-command.ts:10).
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The validated, resolved command (upstream `resolvedCommand`).
#[derive(Debug, Clone)]
pub struct ResolvedRequestHeadersCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub timeout_ms: u64,
}

/// `resolvedCommand` (request-headers-command.ts:143-176): static validation
/// that runs once at config load AND before every request. Env values in
/// `command`/`args`/`env` interpolate (`$env:NAME`).
pub fn resolve_command(config: &Value) -> Result<ResolvedRequestHeadersCommand, String> {
    let Some(object) = config.as_object() else {
        return Err("HTTP request headers command must be an object".to_string());
    };
    let command = match object.get("command").and_then(Value::as_str) {
        Some(command) if !command.trim().is_empty() => crate::utils::interpolate_env_vars(command),
        _ => return Err("HTTP request headers command requires a non-empty command".to_string()),
    };
    let mut args = Vec::new();
    if let Some(list) = object.get("args") {
        let Value::Array(list) = list else {
            return Err("HTTP request headers command args must be strings".to_string());
        };
        for item in list {
            let Some(text) = item.as_str() else {
                return Err("HTTP request headers command args must be strings".to_string());
            };
            args.push(crate::utils::interpolate_env_vars(text));
        }
    }
    let mut env = Vec::new();
    if let Some(map) = object.get("env") {
        let Some(map) = map.as_object() else {
            return Err("HTTP request headers command env values must be strings".to_string());
        };
        for (key, value) in map {
            let Some(text) = value.as_str() else {
                return Err("HTTP request headers command env values must be strings".to_string());
            };
            env.push((key.clone(), crate::utils::interpolate_env_vars(text)));
        }
    }
    let timeout_ms = match object.get("timeoutMs") {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => {
            let Some(number) = value.as_u64() else {
                return Err(
                    "HTTP request headers command timeoutMs must be an integer between 1 and 60000"
                        .to_string(),
                );
            };
            if !(1..=60_000).contains(&number) {
                return Err(
                    "HTTP request headers command timeoutMs must be an integer between 1 and 60000"
                        .to_string(),
                );
            }
            number
        }
    };
    Ok(ResolvedRequestHeadersCommand {
        command,
        args,
        env,
        timeout_ms,
    })
}

/// `HttpRequestCommandEnvelope` (request-headers-command.ts:124-129).
fn envelope(method: &str, url: &str, body_base64: &str) -> Value {
    Value::Object(Map::from_iter([
        ("version".to_string(), Value::from(1)),
        ("method".to_string(), Value::String(method.to_string())),
        ("url".to_string(), Value::String(url.to_string())),
        (
            "bodyBase64".to_string(),
            Value::String(body_base64.to_string()),
        ),
    ]))
}

/// RFC 7230 token check for derived header names (upstream `new Headers`
/// throws on invalid names → "returned an invalid header").
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_valid_header_value(value: &str) -> bool {
    value
        .chars()
        .all(|c| c == '\t' || (c as u32 >= 0x20 && c as u32 != 0x7f))
}

/// `invokeRequestHeadersCommand` (request-headers-command.ts:178-262):
/// spawn, feed the envelope, cap the output, kill on timeout. Returns the
/// derived headers in arrival order (later duplicates override earlier ones
/// at the application site, matching `Headers.set`).
pub async fn invoke_request_headers_command(
    resolved: &ResolvedRequestHeadersCommand,
    method: &str,
    url: &str,
    body_base64: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut command = Command::new(&resolved.command);
    command
        .args(&resolved.args)
        .env_clear()
        .envs(std::env::vars_os())
        .envs(resolved.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // Own process group so a timeout kill reaps the whole tree
        // (stdio.rs precedent; upstream SIGSTOP+ps hardening is Node-only).
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|_| "HTTP request headers command failed to start".to_string())?;
    #[cfg(unix)]
    let group_pid = child.id().unwrap_or(0) as i32;

    // The work future borrows `child`/pipes mutably; on timeout it is
    // dropped (closing the stdin pipe) and the borrow ends so the kill
    // path below can take the child back.
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let payload = serde_json::to_string(&envelope(method, url, body_base64)).unwrap_or_default();

    let work = async {
        // `take()` + explicit drop: tokio's `ChildStdin::shutdown` does not
        // close the pipe — only dropping it delivers EOF to the command.
        if let Some(mut stdin) = stdin.take() {
            let _ = stdin.write_all(payload.as_bytes()).await;
            drop(stdin);
        }
        let mut out: Vec<u8> = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            use tokio::io::AsyncReadExt;
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        out.extend_from_slice(&chunk[..n]);
                        if out.len() > MAX_OUTPUT_BYTES {
                            return Err(
                                "HTTP request headers command output exceeded 64 KiB".to_string()
                            );
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let status = child
            .wait()
            .await
            .map_err(|_| "HTTP request headers command failed to start".to_string())?;
        if !status.success() {
            return Err(format!(
                "HTTP request headers command exited with code {}",
                status.code().unwrap_or(-1)
            ));
        }
        let text = String::from_utf8_lossy(&out).into_owned();
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|_| "HTTP request headers command returned invalid JSON".to_string())?;
        let Value::Object(map) = parsed else {
            return Err("HTTP request headers command must return a JSON object".to_string());
        };
        if map.values().any(|value| !value.is_string()) {
            return Err("HTTP request headers command values must be strings".to_string());
        }
        let headers: Vec<(String, String)> = map
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|v| (name, v.to_string())))
            .collect();
        if headers
            .iter()
            .any(|(name, value)| !is_valid_header_name(name) || !is_valid_header_value(value))
        {
            return Err("HTTP request headers command returned an invalid header".to_string());
        }
        Ok(headers)
    };

    match tokio::time::timeout(Duration::from_millis(resolved.timeout_ms), work).await {
        Ok(inner) => {
            if inner.is_err() {
                kill_group(
                    #[cfg(unix)]
                    group_pid,
                    &mut child,
                );
            }
            inner
        }
        Err(_) => {
            kill_group(
                #[cfg(unix)]
                group_pid,
                &mut child,
            );
            Err(format!(
                "HTTP request headers command timed out after {}ms",
                resolved.timeout_ms
            ))
        }
    }
}

/// Kill the child (whole process group on Unix) — timeout/failure cleanup.
fn kill_group(#[cfg(unix)] group_pid: i32, child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    #[cfg(unix)]
    {
        // SAFETY: negative pid addresses the process group created above.
        unsafe { libc::kill(-group_pid, libc::SIGKILL) };
    }
}

/// Convenience wrapper: resolve + invoke for one request.
pub async fn derive_request_headers(
    config: &Value,
    method: &str,
    url: &str,
    body_base64: &str,
) -> Result<Vec<(String, String)>, String> {
    let resolved = resolve_command(config)?;
    invoke_request_headers_command(&resolved, method, url, body_base64).await
}

/// Body → base64 (envelope encoding).
pub fn body_to_base64(body: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command_config(command: &str) -> Value {
        json!({ "command": command })
    }

    #[test]
    fn resolve_validates_shapes_like_upstream() {
        assert_eq!(
            resolve_command(&json!("nope")).unwrap_err(),
            "HTTP request headers command must be an object"
        );
        assert_eq!(
            resolve_command(&json!({})).unwrap_err(),
            "HTTP request headers command requires a non-empty command"
        );
        assert_eq!(
            resolve_command(&json!({ "command": "  " })).unwrap_err(),
            "HTTP request headers command requires a non-empty command"
        );
        assert_eq!(
            resolve_command(&json!({ "command": "x", "args": [1] })).unwrap_err(),
            "HTTP request headers command args must be strings"
        );
        assert_eq!(
            resolve_command(&json!({ "command": "x", "env": { "A": 1 } })).unwrap_err(),
            "HTTP request headers command env values must be strings"
        );
        assert_eq!(
            resolve_command(&json!({ "command": "x", "timeoutMs": 0 })).unwrap_err(),
            "HTTP request headers command timeoutMs must be an integer between 1 and 60000"
        );
        let resolved = resolve_command(&json!({ "command": "x", "timeoutMs": 60000 })).expect("ok");
        assert_eq!(resolved.timeout_ms, 60_000);
        let default = resolve_command(&command_config("x")).expect("ok");
        assert_eq!(default.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invoke_success_and_fail_closed_paths() {
        // Success: a fixed header object.
        let script = r#"printf '{"X-Derived":"yes"}'"#;
        let headers = invoke_request_headers_command(
            &ResolvedRequestHeadersCommand {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: Vec::new(),
                timeout_ms: 5_000,
            },
            "POST",
            "https://a.test/mcp",
            "",
        )
        .await
        .expect("headers");
        assert_eq!(headers, vec![("X-Derived".to_string(), "yes".to_string())]);

        // Non-zero exit → fail-closed with the exit-code message.
        let error = invoke_request_headers_command(
            &ResolvedRequestHeadersCommand {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "exit 3".to_string()],
                env: Vec::new(),
                timeout_ms: 5_000,
            },
            "POST",
            "https://a.test/mcp",
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "HTTP request headers command exited with code 3".to_string()
        );

        // Invalid JSON → fail-closed.
        let error = invoke_request_headers_command(
            &ResolvedRequestHeadersCommand {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "printf notjson".to_string()],
                env: Vec::new(),
                timeout_ms: 5_000,
            },
            "POST",
            "https://a.test/mcp",
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "HTTP request headers command returned invalid JSON".to_string()
        );

        // Non-string value → fail-closed.
        let error = invoke_request_headers_command(
            &ResolvedRequestHeadersCommand {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), r#"printf '{"X":42}'"#.to_string()],
                env: Vec::new(),
                timeout_ms: 5_000,
            },
            "POST",
            "https://a.test/mcp",
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "HTTP request headers command values must be strings".to_string()
        );

        // Timeout → fail-closed, and no leftover process (kill_on_drop +
        // group kill; the sleep is SIGKILLed so the future returns).
        let start = std::time::Instant::now();
        let error = invoke_request_headers_command(
            &ResolvedRequestHeadersCommand {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "sleep 30".to_string()],
                env: Vec::new(),
                timeout_ms: 200,
            },
            "POST",
            "https://a.test/mcp",
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(error, "HTTP request headers command timed out after 200ms");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout kills fast"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn envelope_reaches_the_command_stdin() {
        // cat echoes stdin back — but cat output IS the envelope, not a
        // header object, so the call fails with the object message; the
        // assertion is that stdin delivery happened (invalid JSON would
        // mean empty stdin).
        let error = invoke_request_headers_command(
            &ResolvedRequestHeadersCommand {
                command: "/bin/cat".to_string(),
                args: Vec::new(),
                env: Vec::new(),
                timeout_ms: 5_000,
            },
            "POST",
            "https://a.test/mcp",
            "aGk=",
        )
        .await
        .unwrap_err();
        // cat echoed the ENVELOPE back — a JSON object whose `version` is a
        // number, proving stdin delivery (empty stdin would give the
        // invalid-JSON arm instead).
        assert_eq!(
            error,
            "HTTP request headers command values must be strings".to_string()
        );
    }

    #[test]
    fn body_base64_encoding() {
        assert_eq!(body_to_base64("hi"), "aGk=");
        assert_eq!(body_to_base64(""), "");
    }
}
