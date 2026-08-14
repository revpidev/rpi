//! Slash commands: `/run`, `/subagents`, `/subagents-doctor` (FR-P0-10).
//!
//! Port of pi-subagents `src/slash/slash-commands.ts` @ v0.48.0 (56f97234)
//! 655-704, P0 subset. `/run` maps to the structured single delegation
//! (ADR-0016: no workflowScript bridge); inline `[output=…]` agent-token
//! config and `[--bg]` are P1 surfaces — `--fork` is kept.

use serde_json::{json, Value};

use crate::tool;

/// Parse `/run` arguments (slash-commands.ts:60-107 subset):
/// `/run <agent>[--fork] <task>` with trailing flag extraction.
#[derive(Debug)]
pub struct RunCommand {
    pub agent: String,
    pub task: String,
    pub fork: bool,
}

pub fn parse_run_args(raw: &str) -> Result<RunCommand, String> {
    let input = raw.trim();
    if input.is_empty() {
        return Err("Usage: /run <agent> [task] [--bg] [--fork]".to_string());
    }
    let mut fork = false;
    let mut tokens: Vec<String> = Vec::new();
    for token in input.split_whitespace() {
        match token {
            "--fork" => fork = true,
            "--bg" => return Err(
                "--bg (background) is not supported in this version; async runs arrive with P1."
                    .to_string(),
            ),
            other => tokens.push(other.to_string()),
        }
    }
    let Some(agent) = tokens.first().cloned() else {
        return Err("Usage: /run <agent> [task] [--bg] [--fork]".to_string());
    };
    let task = tokens[1..].join(" ");
    Ok(RunCommand { agent, task, fork })
}

/// Handle a registered command dispatch. `args` is the raw text after the
/// command name.
pub fn handle_command(
    name: &str,
    args: &str,
    host: &dyn crate::HostContext,
    settings: &crate::config::SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
) -> Value {
    match name {
        "run" => match parse_run_args(args) {
            Ok(run) => {
                let mut params = json!({
                    "agent": run.agent,
                    "task": run.task,
                    "agentScope": "both",
                });
                if run.fork {
                    params["context"] = json!("fork");
                }
                tool::execute_subagent_tool(&params, host, settings, config, runtime)
            }
            Err(usage) => json!({
                "content": [{ "type": "text", "text": usage }],
                "isError": true,
            }),
        },
        "subagents" => {
            let agents =
                crate::agents::discover::discover_agents(&host.cwd(), "both", settings, None)
                    .unwrap_or_default();
            json!({
                "content": [{
                    "type": "text",
                    "text": crate::actions::format_subagents_browser(&agents),
                }],
            })
        }
        "subagents-doctor" => tool::execute_subagent_tool(
            &json!({ "action": "doctor" }),
            host,
            settings,
            config,
            runtime,
        ),
        _ => Value::Null,
    }
}

/// Register the three commands (init-time host calls).
pub fn command_definitions() -> Vec<(String, String)> {
    vec![
        (
            "run".to_string(),
            "Run one subagent in the foreground: /run <agent> [task] [--fork]".to_string(),
        ),
        (
            "subagents".to_string(),
            "List configured subagents (project > user > builtin)".to_string(),
        ),
        (
            "subagents-doctor".to_string(),
            "Show subagent diagnostics (binary, config, discovery, directories)".to_string(),
        ),
    ]
}

/// Convenience for tests: run-command parsing edge cases.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_flags() {
        let run = parse_run_args("scout --fork explore the tree").unwrap();
        assert_eq!(run.agent, "scout");
        assert!(run.fork);
        assert_eq!(run.task, "explore the tree");
        let run = parse_run_args("worker").unwrap();
        assert_eq!(run.task, "");
        assert!(!run.fork);
        assert!(parse_run_args("").is_err());
        assert!(parse_run_args("worker do it --bg")
            .unwrap_err()
            .contains("--bg"));
    }
}
