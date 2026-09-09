//! Parity runner for the subagents golden tests (TE04 G3).
//!
//! Reads the shared fixture JSON (same file the upstream tsx runner reads),
//! produces normalized outputs for `args` (build_rpi_args), `frontmatter`
//! (parse_frontmatter + parse_frontmatter_list), `final-output`
//! (get_final_output) and `fallback` (is_retryable_model_failure; the
//! context-overflow/attempt functions are TE14 and emit null for now),
//! prints one JSON document per line. Invoked by
//! `scripts/subagents-parity/run-parity.mjs`; never part of `cargo test`.
//!
//! Normalization whitelist (documented in scripts/subagents-parity/README.md):
//! prompt/task temp-file paths and `--extension` values become placeholders;
//! the boundary-instruction prepend is a TE-D17 content deviation and is not
//! compared here (argv/env only).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

fn normalize_argv(args: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut skip_value: Option<&str> = None;
    for arg in args {
        if let Some(kind) = skip_value.take() {
            normalized.push(kind.to_string());
            continue;
        }
        match arg.as_str() {
            "--system-prompt" | "--append-system-prompt" => {
                normalized.push(arg.clone());
                skip_value = Some("<PROMPT_FILE>");
            }
            "--extension" => {
                normalized.push(arg.clone());
                skip_value = Some("<EXT>");
            }
            other => {
                if let Some(path) = other.strip_prefix('@') {
                    let _ = path;
                    normalized.push("@<TASK_FILE>".to_string());
                } else {
                    normalized.push(arg.clone());
                }
            }
        }
    }
    normalized
}

fn run_args_case(input: &Value) -> Value {
    let get_str = |key: &str| input.get(key).and_then(Value::as_str);
    let get_list = |key: &str| -> Option<Vec<String>> {
        input.get(key)?.as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    };
    let build_input = rpi_ext_subagents::parity::BuildArgsInputPublic {
        base_args: vec!["--mode".to_string(), "json".to_string(), "-p".to_string()],
        task: get_str("task").unwrap_or_default().to_string(),
        task_delivery: match get_str("taskDelivery") {
            Some("file") => Some(rpi_ext_subagents::parity::TaskDeliveryPublic::File),
            _ => None,
        },
        session_enabled: input.get("sessionEnabled") != Some(&Value::Bool(false)),
        session_dir: get_str("sessionDir").map(PathBuf::from),
        session_file: get_str("sessionFile").map(PathBuf::from),
        model: get_str("model").map(str::to_string),
        thinking: get_str("thinking").map(str::to_string),
        system_prompt: get_str("systemPrompt").map(str::to_string),
        system_prompt_mode: match get_str("systemPromptMode") {
            Some("append") => "append",
            _ => "replace",
        },
        inherit_project_context: input.get("inheritProjectContext") == Some(&Value::Bool(true)),
        inherit_skills: input.get("inheritSkills") == Some(&Value::Bool(true)),
        require_read_tool: input.get("requireReadTool") == Some(&Value::Bool(true)),
        tools: get_list("tools"),
        extensions: get_list("extensions"),
        subagent_only_extensions: get_list("subagentOnlyExtensions"),
        prompt_file_stem: get_str("promptFileStem").map(str::to_string),
        run_id: get_str("runId").map(str::to_string),
        child_agent_name: get_str("childAgentName").map(str::to_string),
        child_index: input
            .get("childIndex")
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        parent_session_id: get_str("parentSessionId").map(str::to_string),
        fanout_authorized: input.get("fanoutAuthorized") == Some(&Value::Bool(true)),
        steer_inbox: None,
        supervisor_channel: None,
        self_extension: Some("/ext/placeholder".to_string()),
    };
    match rpi_ext_subagents::parity::build_args_public(&build_input) {
        Ok(result) => {
            let env: BTreeMap<String, String> = result
                .env
                .iter()
                .filter_map(|(key, value)| value.as_ref().map(|v| (key.clone(), v.clone())))
                .collect();
            json!({
                "ok": true,
                "argv": normalize_argv(&result.args),
                "env": env,
            })
        }
        Err(error) => json!({ "ok": false, "error": error.to_string() }),
    }
}

fn run_frontmatter_case(content: &str) -> Value {
    let parsed = rpi_ext_subagents::parity::parse_frontmatter_public(content);
    let frontmatter: BTreeMap<&str, &str> = parsed
        .frontmatter
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let mut output = json!({
        "frontmatter": frontmatter,
        "body": parsed.body,
    });
    if let Some(raw) = parsed.frontmatter.get("tools") {
        output["tools"] = json!(rpi_ext_subagents::parity::parse_frontmatter_list_public(
            Some(raw)
        ));
    }
    output
}

fn run_final_output_case(messages: &Value) -> Value {
    let messages = messages.as_array().cloned().unwrap_or_default();
    Value::String(rpi_ext_subagents::parity::get_final_output_public(
        &messages,
    ))
}

/// Fallback/replay vectors (target track only, TE13).
///
/// `retryable` drives the ported `isRetryableModelFailure`. `context-overflow`
/// (R7.1.2.2) and `attempt` (R7.1.2.3) have no Rust function until TE14, so the
/// runner emits JSON `null`; the orchestrator turns that into an attributed
/// diff via `expected-target-diffs.json` instead of failing silently.
fn run_fallback_case(case: &Value) -> Value {
    let kind = case.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "retryable" => json!({
            "retryable": rpi_ext_subagents::parity::is_retryable_model_failure_public(
                case.get("error").and_then(Value::as_str),
            ),
        }),
        "context-overflow" => json!({ "contextOverflow": Value::Null }),
        "attempt" => json!({ "attempt": Value::Null }),
        other => json!({ "error": format!("unknown fallback fixture kind: {other}") }),
    }
}

/// Discovery-tree case (target track, TE15 R7.1.3): materialize the case tree
/// under a sandbox user-agent dir with the rpi config-dir name, run the real
/// discovery entry point and emit normalized agents + diagnostics. The
/// upstream leg materializes the same tree with `.pi` and drives v0.66
/// `discoverAgents`; both sides map their config dir to `<CFGDIR>`.
fn run_discovery_case(case: &Value) -> Value {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!(
        "rpi-sub-discovery-parity-{}-{nonce}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let user_dir = root.join("agentdir").join("agents");
    let result = (|| -> Result<Value, String> {
        std::fs::create_dir_all(&user_dir).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(root.join("proj")).map_err(|e| e.to_string())?;
        if let Some(files) = case.pointer("/tree/files").and_then(Value::as_object) {
            for (raw_path, content) in files {
                let relative = raw_path.replace("<CFGDIR>", ".rpi");
                let target = user_dir.join(&relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&target, content.as_str().unwrap_or_default())
                    .map_err(|e| e.to_string())?;
            }
        }
        if let Some(symlinks) = case.pointer("/tree/symlinks").and_then(Value::as_array) {
            #[cfg(unix)]
            for symlink in symlinks {
                let path = symlink.get("path").and_then(Value::as_str).unwrap_or("");
                let target = symlink.get("target").and_then(Value::as_str).unwrap_or("");
                let link = user_dir.join(path);
                if let Some(parent) = link.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::os::unix::fs::symlink(target, &link).map_err(|e| e.to_string())?;
            }
            #[cfg(not(unix))]
            let _ = symlinks;
        }
        let input = rpi_ext_subagents::parity::DiscoveryInputPublic {
            cwd: root.join("proj"),
            scope: "user".to_string(),
            user_dirs: vec![user_dir.clone()],
            builtin_dir: None,
        };
        let (agents, diagnostics) = rpi_ext_subagents::parity::discover_public(&input);
        let relativize = |path: &Path| -> String {
            let relative = path
                .strip_prefix(&user_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            match relative.strip_prefix(".rpi/") {
                Some(rest) => format!("<CFGDIR>/{rest}"),
                None => relative,
            }
        };
        let mut agents: Vec<Value> = agents
            .into_iter()
            // Builtins (embedded `builtin:*` pseudo-paths) are not part of the
            // user-tree parity case; upstream filters its own builtin catalog
            // the same way.
            .filter(|agent| agent.file_path.starts_with(&user_dir))
            .map(|agent| {
                json!({
                    "name": agent.name,
                    "source": agent.source,
                    "path": relativize(&agent.file_path),
                })
            })
            .collect();
        agents.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let mut diagnostics: Vec<Value> = diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic.path.starts_with(&user_dir))
            .map(|diagnostic| {
                json!({
                    "path": relativize(&diagnostic.path),
                    "source": diagnostic.scope,
                    "error": diagnostic.error,
                })
            })
            .collect();
        diagnostics.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(json!({ "agents": agents, "diagnostics": diagnostics }))
    })();
    let _ = std::fs::remove_dir_all(&root);
    match result {
        Ok(output) => output,
        Err(error) => json!({ "error": format!("discovery case setup failed: {error}") }),
    }
}

fn main() {
    let mut raw = String::new();
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let path = args.next().unwrap_or_default();
    let mut file = std::fs::File::open(&path).unwrap_or_else(|error| {
        eprintln!("parity_runner: cannot open {path}: {error}");
        std::process::exit(2);
    });
    file.read_to_string(&mut raw).unwrap_or_else(|error| {
        eprintln!("parity_runner: cannot read {path}: {error}");
        std::process::exit(2);
    });
    let fixtures: Value = serde_json::from_str(&raw).expect("fixture JSON");
    let cases = fixtures
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for case in cases {
        let name = case.get("name").and_then(Value::as_str).unwrap_or("");
        let output = match mode.as_str() {
            "args" => run_args_case(case.get("input").unwrap_or(&Value::Null)),
            "frontmatter" => {
                run_frontmatter_case(case.get("content").and_then(Value::as_str).unwrap_or(""))
            }
            "final-output" => run_final_output_case(case.get("messages").unwrap_or(&Value::Null)),
            "fallback" => run_fallback_case(&case),
            "discovery" => run_discovery_case(&case),
            other => {
                eprintln!("parity_runner: unknown mode {other}");
                std::process::exit(2);
            }
        };
        let line =
            serde_json::to_string(&json!({ "name": name, "output": output })).unwrap_or_default();
        println!("{line}");
    }
}
