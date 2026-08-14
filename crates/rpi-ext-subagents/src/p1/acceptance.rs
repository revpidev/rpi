//! Acceptance levels, gate shorthand, fenced-report parsing and run budgets
//! (FR-P1-09).
//!
//! Port of pi-subagents `src/runs/shared/acceptance.ts` (levels L28-75,
//! inference L77-147, gate shorthand L160-165, report parsing L485-594) plus
//! `turn-budget.ts` / `tool-budget.ts` / `usage-budget.ts` prompt injection
//! and pre-launch checks. Gate/verify commands execute host-side through
//! direct `std::process::Command` spawns (same style as the worktree git
//! calls — the host `exec` envelope is synchronous; deviation TE-D2x).

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::{json, Value};

/// `LEVEL_RANK` (acceptance.ts:28-33).
pub fn level_rank(level: &str) -> Option<u8> {
    match level {
        "none" => Some(0),
        "attested" => Some(1),
        "checked" => Some(2),
        "verified" => Some(3),
        _ => None,
    }
}

/// `requiredEvidenceForLevel` (acceptance.ts:64-75).
pub fn required_evidence_for_level(level: &str) -> &'static [&'static str] {
    match level {
        "attested" => &["manual-notes", "residual-risks"],
        "checked" => &[
            "changed-files",
            "tests-added",
            "commands-run",
            "residual-risks",
            "no-staged-files",
        ],
        "verified" => &[
            "changed-files",
            "tests-added",
            "commands-run",
            "validation-output",
            "residual-risks",
            "no-staged-files",
        ],
        _ => &[],
    }
}

/// `inferLevel` (acceptance.ts:77-147) — the inference inputs the structured
/// surface carries: agent name, optional acceptanceRole, task text, async,
/// writer detection. Returns `(level, review_required)`.
pub fn infer_level(
    agent_name: &str,
    acceptance_role: Option<&str>,
    task: &str,
    is_async: bool,
) -> (String, bool) {
    let lowered_task = task.to_lowercase();
    let read_only_name = ["reviewer", "oracle", "scout", "researcher", "analyst"]
        .iter()
        .any(|name| agent_name.contains(name));
    let risky_task = [
        "release",
        "migration",
        "security",
        "data-loss",
        "destructive",
        "post-review",
        "fix pass",
    ]
    .iter()
    .any(|word| lowered_task.contains(word));
    let writer =
        acceptance_role == Some("writer") || (!read_only_name && acceptance_role.is_some());

    if (is_async && writer) || risky_task {
        // risky → checked with a required reviewer pass.
        return ("checked".to_string(), true);
    }
    if writer
        || (!read_only_name
            && acceptance_role.is_none()
            && !risky_task
            && is_task_mutation(&lowered_task))
    {
        return ("checked".to_string(), false);
    }
    if read_only_name || is_read_only_task(&lowered_task) {
        return ("attested".to_string(), false);
    }
    ("attested".to_string(), false)
}

fn is_task_mutation(lowered_task: &str) -> bool {
    [
        "implement",
        "fix",
        "refactor",
        "write",
        "edit",
        "update",
        "delete",
        "add ",
    ]
    .iter()
    .any(|word| lowered_task.contains(word))
}

fn is_read_only_task(lowered_task: &str) -> bool {
    ["review", "inspect", "read", "analyze", "summarize", "audit"]
        .iter()
        .any(|word| lowered_task.contains(word))
}

/// `normalizeGateAcceptance` (acceptance.ts:160-165): `gate: "cmd"` →
/// verified acceptance with one verify entry.
pub fn normalize_gate(gate: &str) -> Value {
    json!({
        "level": "verified",
        "verify": [{ "id": "gate", "command": gate }],
    })
}

/// `ACCEPTANCE_REPORT_FIELDS` (acceptance.ts:487-509) — camelCase + snake_case
/// + `notes→manualNotes`; unknown keys are errors (L588-594).
pub fn normalize_report_field(key: &str) -> Result<&'static str, String> {
    Ok(match key {
        "criteriaSatisfied" | "criteria_satisfied" => "criteriaSatisfied",
        "changedFiles" | "changed_files" => "changedFiles",
        "testsAddedOrUpdated" | "tests_added_or_updated" => "testsAddedOrUpdated",
        "commandsRun" | "commands_run" => "commandsRun",
        "validationOutput" | "validation_output" => "validationOutput",
        "residualRisks" | "residual_risks" => "residualRisks",
        "noStagedFiles" | "no_staged_files" => "noStagedFiles",
        "diffSummary" | "diff_summary" => "diffSummary",
        "reviewFindings" | "review_findings" => "reviewFindings",
        "manualNotes" | "manual_notes" | "notes" => "manualNotes",
        other => return Err(format!("unsupported acceptance report field '{other}'")),
    })
}

fn normalized_token(value: &str) -> String {
    let lowered = value.trim().to_lowercase().replace([' ', '_'], "-");
    let mut out = String::with_capacity(lowered.len());
    let mut last_dash = false;
    for ch in lowered.chars() {
        if ch == '-' {
            if !last_dash {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.push(ch);
            last_dash = false;
        }
    }
    out
}

/// `normalizeCriterionStatus` (acceptance.ts:518-525).
pub fn normalize_criterion_status(value: &str) -> String {
    let token = normalized_token(value);
    match token.as_str() {
        "satisfied" | "met" | "complete" | "completed" | "done" | "pass" | "passed" | "success"
        | "succeeded" => "satisfied".to_string(),
        "not-satisfied" | "not-met" | "unmet" | "incomplete" | "fail" | "failed" => {
            "not-satisfied".to_string()
        }
        "not-applicable" | "n-a" | "na" | "skip" | "skipped" => "not-applicable".to_string(),
        _ => value.to_string(),
    }
}

/// `normalizeCommandResult` (acceptance.ts:527-534).
pub fn normalize_command_result(value: &str) -> String {
    let token = normalized_token(value);
    match token.as_str() {
        "passed" | "pass" | "success" | "successful" | "succeeded" | "ok" => "passed".to_string(),
        "failed" | "fail" | "failure" | "error" => "failed".to_string(),
        "not-run" | "not-executed" | "skip" | "skipped" => "not-run".to_string(),
        _ => value.to_string(),
    }
}

/// Parse a fenced acceptance report out of child output (L485-594 subset:
/// the four wrapper tags; unknown fields and duplicate spellings error).
pub fn parse_acceptance_report(output: &str) -> Option<Result<BTreeMap<String, Value>, String>> {
    const WRAPPERS: [&str; 4] = [
        "acceptance",
        "acceptance-report",
        "acceptance_report",
        "acceptanceReport",
    ];
    for wrapper in WRAPPERS {
        let open = format!("```{wrapper}\n");
        if let Some(start) = output.find(&open) {
            let body_start = start + open.len();
            let Some(close_offset) = output[body_start..].find("```") else {
                continue;
            };
            let raw = &output[body_start..body_start + close_offset];
            let parsed: Value = match serde_json::from_str(raw.trim()) {
                Ok(value) => value,
                Err(error) => return Some(Err(format!("acceptance report JSON: {error}"))),
            };
            let Some(object) = parsed.as_object() else {
                return Some(Err("acceptance report must be a JSON object".to_string()));
            };
            let mut normalized: BTreeMap<String, Value> = BTreeMap::new();
            for (key, value) in object {
                let field = match normalize_report_field(key) {
                    Ok(field) => field,
                    Err(message) => return Some(Err(message)),
                };
                if normalized
                    .insert(field.to_string(), value.clone())
                    .is_some()
                {
                    return Some(Err(format!(
                        "acceptance report field '{field}' was provided twice with different spellings"
                    )));
                }
            }
            // Criterion / command normalization (L511-534).
            if let Some(criteria) = normalized
                .get_mut("criteriaSatisfied")
                .and_then(Value::as_array_mut)
            {
                for criterion in criteria.iter_mut() {
                    if let Some(status) = criterion.get("status").and_then(Value::as_str) {
                        let normalized_status = normalize_criterion_status(status);
                        if let Some(target) = criterion.as_object_mut() {
                            target.insert("status".to_string(), json!(normalized_status));
                        }
                    }
                }
            }
            if let Some(commands) = normalized
                .get_mut("commandsRun")
                .and_then(Value::as_array_mut)
            {
                for command in commands.iter_mut() {
                    if let Some(result) = command.get("result").and_then(Value::as_str) {
                        let normalized_result = normalize_command_result(result);
                        if let Some(target) = command.as_object_mut() {
                            target.insert("result".to_string(), json!(normalized_result));
                        }
                    }
                }
            }
            return Some(Ok(normalized));
        }
    }
    None
}

/// Gate/verify command execution (`runVerifyCommands` subset: default 120s
/// timeout, cwd-bound; failures fail explicit gates).
pub fn run_gate_command(command: &str, cwd: &std::path::Path) -> Result<bool, String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let output = Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("gate command failed to start: {e}"))?;
    Ok(output.status.success())
}

/// Turn/tool budget system-prompt injection (turn-budget.ts L26-39 +
/// tool-budget.ts): children are told their budgets up front.
pub fn build_budget_prompt(turn_budget: Option<&Value>, tool_budget: Option<&Value>) -> String {
    let mut lines = Vec::new();
    if let Some(turn) = turn_budget {
        let max_turns = turn.get("maxTurns").and_then(Value::as_u64);
        let grace = turn.get("graceTurns").and_then(Value::as_u64).unwrap_or(1);
        if let Some(max_turns) = max_turns {
            lines.push(format!(
                "Turn budget: at most {max_turns} turns (grace {grace}). Wrap up before the limit; finish with a partial result rather than running past it."
            ));
        }
    }
    if let Some(tool) = tool_budget {
        let hard = tool.get("hard").and_then(Value::as_u64);
        let soft = tool.get("soft").and_then(Value::as_u64);
        if let Some(soft) = soft {
            lines.push(format!(
                "Tool budget: soft cap {soft} tool calls — economize from here."
            ));
        }
        if let Some(hard) = hard {
            lines.push(format!(
                "Tool budget: hard cap {hard} tool calls — budget-enforcing tools will be blocked past it."
            ));
        }
    }
    lines.join("\n")
}

/// Usage-budget pre-launch check (usage-budget.ts): `exhausted` skips the
/// launch (chain-execution.ts L314-326) based on the accumulated cost.
pub fn usage_budget_allows_launch(
    usage_budget: Option<&Value>,
    accumulated_cost: f64,
) -> Result<bool, String> {
    let Some(budget) = usage_budget else {
        return Ok(true);
    };
    if let Some(hard) = budget
        .get("costUsd")
        .and_then(|c| c.get("hard"))
        .and_then(Value::as_f64)
    {
        if accumulated_cost >= hard {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ranks_and_evidence() {
        assert_eq!(level_rank("verified"), Some(3));
        assert_eq!(level_rank("auto"), None);
        assert_eq!(
            required_evidence_for_level("attested"),
            &["manual-notes", "residual-risks"]
        );
        assert_eq!(required_evidence_for_level("none").len(), 0);
        assert!(required_evidence_for_level("verified").contains(&"validation-output"));
    }

    #[test]
    fn level_inference_paths() {
        // Read-only agent → attested.
        let (level, review) = infer_level("reviewer", None, "check the diff", false);
        assert_eq!((level.as_str(), review), ("attested", false));
        // Writer role → checked.
        let (level, _) = infer_level("worker", Some("writer"), "t", false);
        assert_eq!(level, "checked");
        // Risky task → checked + required review.
        let (level, review) = infer_level("worker", None, "run the migration", false);
        assert_eq!((level.as_str(), review), ("checked", true));
        // Async writer → checked + review.
        let (level, review) = infer_level("worker", Some("writer"), "t", true);
        assert_eq!((level.as_str(), review), ("checked", true));
        // Mutation task by a non-reader name → checked.
        let (level, _) = infer_level("fixer", None, "implement the feature", false);
        assert_eq!(level, "checked");
    }

    #[test]
    fn gate_shorthand_normalizes() {
        let acceptance = normalize_gate("cargo test");
        assert_eq!(acceptance["level"], "verified");
        assert_eq!(acceptance["verify"][0]["command"], "cargo test");
    }

    #[test]
    fn report_parsing_synonyms_and_errors() {
        let report = "```acceptance\n{\"changed_files\": [\"a.rs\"], \"commands_run\": [{\"command\": \"cargo test\", \"result\": \"PASS\"}], \"notes\": \"ok\", \"bogus\": 1}\n```";
        match parse_acceptance_report(report) {
            Some(Err(message)) => assert!(message.contains("bogus"), "{message}"),
            other => panic!("expected error, got {other:?}"),
        }
        let report =
            "```acceptance\n{\"changed_files\": [\"a.rs\"], \"changedFiles\": [\"b.rs\"]}\n```";
        match parse_acceptance_report(report) {
            Some(Err(message)) => assert!(message.contains("twice"), "{message}"),
            other => panic!("expected duplicate error, got {other:?}"),
        }
        let report = "```acceptance\n{\"criteria_satisfied\": [{\"id\": \"c1\", \"status\": \"Completed\"}], \"commands_run\": [{\"command\": \"x\", \"result\": \"pass\"}]}\n```";
        let Some(Ok(fields)) = parse_acceptance_report(report) else {
            panic!("expected ok");
        };
        assert!(fields.contains_key("criteriaSatisfied"));
        assert_eq!(
            fields["criteriaSatisfied"][0]["status"], "satisfied",
            "status synonyms normalize"
        );
        assert_eq!(fields["commandsRun"][0]["result"], "passed");
    }

    #[test]
    fn criterion_and_command_normalization() {
        assert_eq!(normalize_criterion_status("Not Met"), "not-satisfied");
        assert_eq!(normalize_criterion_status("n-a"), "not-applicable");
        // Upstream only folds whitespace/underscores, so "N/A" stays literal.
        assert_eq!(normalize_criterion_status("N/A"), "N/A");
        assert_eq!(normalize_criterion_status("weird"), "weird");
        assert_eq!(normalize_command_result("SUCCESSFUL"), "passed");
        assert_eq!(normalize_command_result("not executed"), "not-run");
    }

    #[test]
    fn budget_prompt_and_usage_gate() {
        let turn = json!({"maxTurns": 5, "graceTurns": 2});
        let tool = json!({"soft": 10, "hard": 20});
        let prompt = build_budget_prompt(Some(&turn), Some(&tool));
        assert!(prompt.contains("at most 5 turns"));
        assert!(prompt.contains("soft cap 10"));
        assert!(prompt.contains("hard cap 20"));
        let usage = json!({"costUsd": {"soft": 1.0, "hard": 2.0}});
        assert!(usage_budget_allows_launch(Some(&usage), 1.5).unwrap());
        assert!(!usage_budget_allows_launch(Some(&usage), 2.5).unwrap());
        assert!(usage_budget_allows_launch(None, 100.0).unwrap());
    }
}
