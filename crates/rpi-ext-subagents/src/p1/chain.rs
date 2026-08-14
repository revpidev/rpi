//! Chain (sequential) runs (FR-P1-02 / FR-P1-03): `steps: [...]` executes
//! children in order, feeding each step the previous step's output via
//! `{previous}`/`{task}`/`{outputs.<name>}` interpolation, with step-level
//! output/reads/skill overrides, a per-run scratch directory, and
//! stop-on-failure returning completed steps.
//!
//! Port of pi-subagents `src/runs/foreground/chain-execution.ts` (sequential
//! step loop + interpolation L1322-1333), `src/shared/settings.ts`
//! (`resolveStepBehavior` L272-299, `buildChainInstructions` L377-425,
//! `resolveChainPath` L351) and `src/runs/shared/chain-outputs.ts`
//! (`{outputs.name}` bindings).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::agents::discover::{self, AgentConfig, ContextMode};
use crate::p1::launch_child::{self, ChildSpec, OutputOverride, RunCtx};
use crate::PluginRuntime;

/// `CHAIN_DIR_MAX_AGE_MS` (settings.ts:11): 24h scratch retention.
pub const CHAIN_DIR_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

const INITIAL_PROGRESS_CONTENT: &str = "# Chain Progress\n\n## Status\n\n- Started\n";

#[derive(Debug, Clone)]
pub struct StepSpec {
    pub agent_name: String,
    /// Raw task template; `{task}`/`{previous}`/`{outputs.x}` interpolated at
    /// execution. Empty → default template (`{task}` first, `{previous}` rest).
    pub task_template: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub context: Option<ContextMode>,
    pub output: OutputOverride,
    pub reads: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub progress: bool,
    /// Named output binding (`as:`) for `{outputs.<name>}` references.
    pub binding: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// Parse and validate the `steps` array.
pub fn parse_steps(steps: &Value) -> Result<Vec<StepSpec>, String> {
    let Some(items) = steps.as_array() else {
        return Err("steps must be an array of step objects.".to_string());
    };
    if items.is_empty() {
        return Err("steps must contain at least one step.".to_string());
    }
    let mut specs = Vec::new();
    let mut bindings = std::collections::BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!("steps[{index}] must be an object."));
        };
        let agent_name = object
            .get("agent")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("steps[{index}] is missing a non-empty agent."))?;
        let binding = object
            .get("as")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        if let Some(binding) = &binding {
            if !bindings.insert(binding.clone()) {
                return Err(format!(
                    "Duplicate output binding '{binding}' in steps; each `as` name must be unique."
                ));
            }
        }
        let output = match object.get("output") {
            Some(Value::Bool(false)) => OutputOverride::Disabled,
            Some(Value::String(path)) if !path.trim().is_empty() => {
                OutputOverride::Path(crate::paths::expand_tilde_and_resolve(path))
            }
            _ => OutputOverride::Inherit,
        };
        let reads = match object.get("reads") {
            Some(Value::String(raw)) => Some(
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<String>>(),
            ),
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            ),
            _ => None,
        };
        let skills = match object.get("skill") {
            Some(Value::String(raw)) => Some(
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<String>>(),
            ),
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            ),
            _ => None,
        };
        specs.push(StepSpec {
            agent_name: agent_name.to_string(),
            task_template: object
                .get("task")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            model: object
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            thinking: object
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            context: match object.get("context").and_then(Value::as_str) {
                Some("fork") => Some(ContextMode::Fork),
                Some("fresh") => Some(ContextMode::Fresh),
                Some(_) => Some(ContextMode::Fresh),
                None => None,
            },
            output,
            reads,
            skills,
            progress: object
                .get("progress")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            binding,
            timeout_ms: match object.get("timeoutMs") {
                Some(Value::Number(n)) if n.is_u64() && n.as_u64().unwrap_or(0) > 0 => n.as_u64(),
                _ => None,
            },
        });
    }
    Ok(specs)
}

/// `getProjectChainRunsDir` (artifacts.ts:142): `<cwd>/.rpi/subagents/chain-runs`.
pub fn project_chain_runs_dir(cwd: &Path) -> PathBuf {
    crate::paths::get_project_config_dir(cwd)
        .join("subagents")
        .join("chain-runs")
}

/// `createChainDir` (settings.ts:188): scratch root by artifact preference —
/// project scope under `.rpi/subagents/chain-runs/<runId>`, temp scope under
/// the plugin temp root.
pub fn create_chain_dir(ctx: &RunCtx) -> PathBuf {
    let root = match ctx.config.artifact_dir_preference() {
        "temp" => crate::paths::temp_root_dir().join("chain-runs"),
        _ => project_chain_runs_dir(&ctx.base_cwd),
    };
    let dir = root.join(&ctx.run_id);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// `cleanupOldChainDirs` (settings.ts:197-215): drop chain dirs older than
/// 24h from the given root.
pub fn cleanup_old_chain_dirs(root: &Path, max_age_ms: u64) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = crate::artifacts::now_millis();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if now.saturating_sub(modified) > max_age_ms {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// `resolveChainPath` (settings.ts:351): `~` expansion, absolute passthrough,
/// relative against the chain dir.
pub fn resolve_chain_path(file_path: &str, chain_dir: &Path) -> PathBuf {
    let expanded = crate::paths::expand_tilde_and_resolve(file_path);
    if expanded.is_absolute() {
        expanded
    } else {
        chain_dir.join(expanded)
    }
}

/// `resolveExistingReadInstructionPaths` (settings.ts:347): only reads that
/// exist survive into the instruction.
fn existing_read_paths(reads: &[String], chain_dir: &Path) -> Vec<PathBuf> {
    reads
        .iter()
        .map(|r| resolve_chain_path(r, chain_dir))
        .filter(|p| p.exists())
        .collect()
}

/// `{outputs.name}` references (`OUTPUT_REF_PATTERN`, chain-outputs.ts:5).
fn resolve_output_references(template: &str, outputs: &BTreeMap<String, String>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{outputs.") {
        result.push_str(&rest[..start]);
        let after = &rest[start + "{outputs.".len()..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match outputs.get(name) {
                    Some(value) => result.push_str(value),
                    None => result.push_str(&rest[start..start + "{outputs.".len() + end + 1]),
                }
                rest = &after[end + 1..];
            }
            None => {
                result.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    result.push_str(rest);
    result
}

/// Interpolation order (chain-execution.ts L1322-1333 + L339-353):
/// `{outputs.x}` first, then `{task}`, then `{previous}`, then `{chain_dir}`.
pub fn interpolate_task(
    template: &str,
    default_template: &str,
    original_task: &str,
    previous_output: Option<&str>,
    outputs: &BTreeMap<String, String>,
    chain_dir: &Path,
) -> String {
    let effective = if template.trim().is_empty() {
        default_template
    } else {
        template
    };
    let referenced = resolve_output_references(effective, outputs);
    let previous = previous_output.unwrap_or("");
    referenced
        .replace("{task}", original_task)
        .replace("{previous}", previous)
        .replace("{chain_dir}", &chain_dir.to_string_lossy())
}

/// `buildChainInstructions` (settings.ts:377-425): read/output prefix lines
/// and progress/previous-output suffix.
pub fn build_chain_instructions(
    reads: &[String],
    output_path: Option<&Path>,
    progress: bool,
    is_first_progress_agent: bool,
    previous_summary: Option<&str>,
    chain_dir: &Path,
) -> (String, String) {
    let mut prefix_parts: Vec<String> = Vec::new();
    let mut suffix_parts: Vec<String> = Vec::new();
    let files = existing_read_paths(reads, chain_dir);
    if !files.is_empty() {
        let joined = files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect::<Vec<String>>()
            .join(", ");
        prefix_parts.push(format!("[Read from: {joined}]"));
    }
    if let Some(output) = output_path {
        prefix_parts.push(format!("[Write to: {}]", output.to_string_lossy()));
    }
    if progress {
        let progress_path = chain_dir.join("progress.md");
        if is_first_progress_agent {
            suffix_parts.push(format!(
                "Create and maintain progress at: {}",
                progress_path.to_string_lossy()
            ));
        } else {
            suffix_parts.push(format!(
                "Update progress at: {}",
                progress_path.to_string_lossy()
            ));
        }
    }
    if let Some(summary) = previous_summary {
        let trimmed = summary.trim();
        if !trimmed.is_empty() {
            suffix_parts.push(format!("Previous step output:\n{trimmed}"));
        }
    }
    let prefix = if prefix_parts.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", prefix_parts.join("\n"))
    };
    let suffix = if suffix_parts.is_empty() {
        String::new()
    } else {
        format!("\n\n---\n{}", suffix_parts.join("\n"))
    };
    (prefix, suffix)
}

/// One executed step's result.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub index: usize,
    pub agent: String,
    pub output: String,
    pub exit_code: i32,
    pub error: Option<String>,
    pub details: Value,
}

/// Run the chain sequentially (chain-execution.ts executeChain sequential
/// loop, P1 structured subset): stop on first failure, return every completed
/// step plus the failure.
pub fn run_chain(
    steps: &[StepSpec],
    agents: &[AgentConfig],
    ctx: &RunCtx,
    runtime: &PluginRuntime,
    original_task: &str,
) -> Result<(Vec<StepOutcome>, Option<StepOutcome>), String> {
    runtime.block_on(run_chain_async(steps, agents, ctx, original_task))
}

/// Async core (see [`run_chain`]) — direct call from runtime tasks.
pub async fn run_chain_async(
    steps: &[StepSpec],
    agents: &[AgentConfig],
    ctx: &RunCtx,
    original_task: &str,
) -> Result<(Vec<StepOutcome>, Option<StepOutcome>), String> {
    let chain_dir = create_chain_dir(ctx);
    let progress_requested = steps.iter().any(|s| s.progress);
    if progress_requested {
        // `writeInitialProgressFile` (settings.ts:371).
        let _ = std::fs::create_dir_all(&chain_dir);
        let _ = std::fs::write(chain_dir.join("progress.md"), INITIAL_PROGRESS_CONTENT);
    }
    let usage_budget = ctx.usage_budget.clone();
    let mut accumulated_cost = 0.0f64;
    let mut outputs: BTreeMap<String, String> = BTreeMap::new();
    let mut completed: Vec<StepOutcome> = Vec::new();
    let mut previous_output: Option<String> = None;
    let mut first_progress_seen = false;
    for (index, step) in steps.iter().enumerate() {
        let agent = discover::resolve_agent_name(agents, &step.agent_name)?
            .cloned()
            .ok_or_else(|| format!("Unknown agent: {}", step.agent_name))?;
        let default_template = if index == 0 { "{task}" } else { "{previous}" };
        let task = interpolate_task(
            &step.task_template,
            default_template,
            original_task,
            previous_output.as_deref(),
            &outputs,
            &chain_dir,
        );
        // Step-level behavior resolution (resolveStepBehavior L272-299):
        // step override > agent frontmatter > false.
        let output_override = match &step.output {
            OutputOverride::Path(path) => {
                Some(resolve_chain_path(&path.to_string_lossy(), &chain_dir))
            }
            OutputOverride::Disabled => None,
            OutputOverride::Inherit => agent
                .output
                .as_deref()
                .map(|raw| resolve_chain_path(raw, &chain_dir)),
        };
        let reads: Vec<String> = step
            .reads
            .clone()
            .unwrap_or_else(|| agent.default_reads.clone());
        let skills = step.skills.clone().unwrap_or_else(|| agent.skills.clone());
        let progress = step.progress || agent.default_progress;
        let is_first_progress = progress && !first_progress_seen;
        if progress {
            first_progress_seen = true;
        }
        let (prefix, suffix) = build_chain_instructions(
            &reads,
            output_override.as_deref(),
            progress,
            is_first_progress,
            previous_output.as_deref(),
            &chain_dir,
        );
        let full_task = format!("{prefix}{task}{suffix}");
        let spec = ChildSpec {
            agent_name: agent.name.clone(),
            task: full_task,
            model: step.model.clone(),
            thinking: step.thinking.clone(),
            context: step.context,
            cwd: None,
            output: match output_override {
                Some(path) => OutputOverride::Path(path),
                None => OutputOverride::Disabled,
            },
            timeout_ms: step.timeout_ms,
            child_index: index as u32,
            skills: Some(skills),
            gate: None,
            turn_budget: None,
            tool_budget: None,
            session_file: None,
            steer_inbox: None,
            skill_fallback_cwd: Some(ctx.base_cwd.clone()),
            skill_primary_cwd: Some(chain_dir.clone()),
        };
        // Usage-budget gate (FR-P1-09, usage-budget.ts): exhausted budgets
        // skip the launch instead of running over.
        if !crate::p1::acceptance::usage_budget_allows_launch(
            usage_budget.as_ref(),
            accumulated_cost,
        )? {
            break;
        }
        let outcome = launch_child::run_child_async(&spec, &agent, ctx).await?;
        accumulated_cost += outcome
            .result
            .usage
            .get("cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let mut details = json!({
            "index": index,
            "agent": outcome.agent_name,
            "task": "[prompt redacted]",
            "context": outcome.context.as_str(),
            "exitCode": outcome.result.exit_code,
            "usage": outcome.result.usage,
            "timedOut": outcome.result.timed_out,
        });
        if let Some(error) = &outcome.result.error {
            details["error"] = json!(error);
        }
        if let Some(paths) = &outcome.result.artifact_paths {
            details["artifactPaths"] = paths.to_json();
        }
        details["finalOutput"] = json!(outcome.result.final_output);
        if let Some(saved) = &outcome.saved_output_path {
            details["savedOutputPath"] = json!(saved.to_string_lossy());
        }
        let step_outcome = StepOutcome {
            index,
            agent: outcome.agent_name.clone(),
            output: outcome.result.final_output.clone(),
            exit_code: outcome.result.exit_code,
            error: outcome.result.error.clone(),
            details,
        };
        if outcome.result.exit_code != 0 {
            return Ok((completed, Some(step_outcome)));
        }
        if let Some(binding) = &step.binding {
            outputs.insert(binding.clone(), outcome.result.final_output.clone());
        }
        previous_output = Some(outcome.result.final_output);
        completed.push(step_outcome);
    }
    Ok((completed, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_references_resolve() {
        let mut outputs = BTreeMap::new();
        outputs.insert("scan".to_string(), "the scan".to_string());
        let text =
            resolve_output_references("Combine {outputs.scan} and {outputs.missing}", &outputs);
        assert_eq!(text, "Combine the scan and {outputs.missing}");
        // Unclosed reference passes through untouched.
        let text = resolve_output_references("tail {outputs.scan", &outputs);
        assert_eq!(text, "tail {outputs.scan");
    }

    #[test]
    fn interpolation_order_and_defaults() {
        let mut outputs = BTreeMap::new();
        outputs.insert("ctx".to_string(), "CONTEXT".to_string());
        let chain_dir = PathBuf::from("/tmp/chain");
        // Empty template → default ({task} first step, {previous} later).
        let text = interpolate_task("", "{task}", "ORIGINAL", None, &outputs, &chain_dir);
        assert_eq!(text, "ORIGINAL");
        let text = interpolate_task(
            "",
            "{previous}",
            "ORIGINAL",
            Some("PREV"),
            &outputs,
            &chain_dir,
        );
        assert_eq!(text, "PREV");
        // Full stack: outputs → task → previous → chain_dir.
        let text = interpolate_task(
            "{outputs.ctx} | {task} | {previous} | {chain_dir}",
            "{task}",
            "T",
            Some("P"),
            &outputs,
            &chain_dir,
        );
        assert_eq!(text, format!("CONTEXT | T | P | /tmp/chain"));
    }

    #[test]
    fn chain_instructions_prefix_and_suffix() {
        let chain_dir = PathBuf::from("/tmp/chain-x");
        std::fs::create_dir_all(&chain_dir).unwrap();
        std::fs::write(chain_dir.join("context.md"), "x").unwrap();
        let (prefix, suffix) = build_chain_instructions(
            &["context.md".to_string(), "absent.md".to_string()],
            Some(&chain_dir.join("plan.md")),
            true,
            true,
            Some(" prev "),
            &chain_dir,
        );
        assert!(prefix.starts_with("[Read from: "));
        assert!(prefix.contains("context.md"));
        assert!(!prefix.contains("absent.md"));
        assert!(prefix.contains("[Write to: /tmp/chain-x/plan.md]"));
        assert!(prefix.ends_with("\n\n"));
        assert!(suffix.starts_with("\n\n---\n"));
        assert!(suffix.contains("Create and maintain progress at:"));
        assert!(suffix.contains("Previous step output:\nprev"));
        let _ = std::fs::remove_dir_all(&chain_dir);
    }

    #[test]
    fn step_parsing_and_binding_validation() {
        let steps = json!([
            {"agent": "scout"},
            {"agent": "worker", "as": "plan", "reads": "a.md, b.md", "skill": "s1,s2"},
            {"agent": "worker", "as": "plan"}
        ]);
        assert!(parse_steps(&steps).is_err());
        let steps = json!([
            {"agent": "scout"},
            {"agent": "worker", "as": "plan", "reads": "a.md, b.md", "skill": "s1,s2"}
        ]);
        let parsed = parse_steps(&steps).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].binding.as_deref(), Some("plan"));
        assert_eq!(parsed[1].reads.as_deref().map(|v| v.len()), Some(2));
        assert_eq!(parsed[1].skills.as_deref().map(|v| v.len()), Some(2));
        assert!(parse_steps(&json!([])).is_err());
        assert!(parse_steps(&json!([{"task": "no agent"}])).is_err());
    }
}
