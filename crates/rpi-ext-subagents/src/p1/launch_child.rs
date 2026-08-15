//! Shared per-child launch preparation: the single (P0), parallel, chain and
//! async (TE05) execution paths all resolve a child the same way —
//! timeout/context precedence, fresh/fork session handling, model+thinking
//! chain with fuzzy resolution and scope checks, skill injection, spawn-cap
//! accounting, and the `ForegroundRunInput` assembly (FR-P1-01/02/04/05).
//!
//! Extracted from the P0 single-run path in `tool.rs` (executor
//! `runSingleSubagent` flow, subagent-executor.ts:5450-5700); the single path
//! now routes through here so all four consumers cannot drift.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::agents::discover::{self, AgentConfig, ContextMode};
use crate::agents::skills;
use crate::config::{ExtensionConfig, SettingsPair};
use crate::launch::model::{self, AvailableModel};
use crate::runner::budget;
use crate::runner::foreground::{self, ForegroundRunInput, ForegroundRunResult};
use crate::{session_fork, PluginRuntime};

/// Per-child output override (`normalizeOutputOverride` chain step
/// semantics): a path, disabled, or inherit the agent default.
#[derive(Debug, Clone, Default)]
pub enum OutputOverride {
    Path(PathBuf),
    Disabled,
    #[default]
    Inherit,
}

/// One child to launch — already agent-resolved with the final task text
/// (chain interpolation and instruction prefixes applied by the caller).
#[derive(Debug, Clone, Default)]
pub struct ChildSpec {
    pub agent_name: String,
    pub task: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub context: Option<ContextMode>,
    pub cwd: Option<PathBuf>,
    pub output: OutputOverride,
    pub timeout_ms: Option<u64>,
    pub child_index: u32,
    /// Explicit skill list (chain step `skill`); `None` inherits agent skills.
    pub skills: Option<Vec<String>>,
    /// Resume: launch the child against an existing session file
    /// (`--session <file>`, FR-P1-04 resume semantics).
    pub session_file: Option<PathBuf>,
    /// Steer inbox dir (FR-P1-04): background children poll it for injected
    /// messages; `None` clears the env (foreground runs).
    pub steer_inbox: Option<PathBuf>,
    /// Acceptance gate command (FR-P1-09): runs host-side after the child;
    /// a failing gate fails the run. Inferred gates only record.
    pub gate: Option<String>,
    /// Budget payloads inherited from the top-level call (FR-P1-09).
    pub turn_budget: Option<Value>,
    pub tool_budget: Option<Value>,
    /// Skill resolution fallback cwd (chain scratch dir); defaults to base.
    pub skill_fallback_cwd: Option<PathBuf>,
    /// Extra cwd for skill resolution when the chain dir differs.
    pub skill_primary_cwd: Option<PathBuf>,
}

impl ChildSpec {
    /// Single-run spec from raw tool params (`{agent, task, ...}`).
    pub fn from_params(object: &serde_json::Map<String, Value>) -> Self {
        let output = match object.get("output") {
            Some(Value::Bool(false)) => OutputOverride::Disabled,
            Some(Value::String(path)) if !path.trim().is_empty() => {
                OutputOverride::Path(crate::paths::expand_tilde_and_resolve(path))
            }
            _ => OutputOverride::Inherit,
        };
        Self {
            agent_name: object
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            task: object
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
            cwd: object
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(crate::paths::expand_tilde_and_resolve),
            output,
            timeout_ms: resolve_call_timeout(object),
            child_index: 0,
            skills: None,
            gate: object
                .get("gate")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            turn_budget: object.get("turnBudget").cloned(),
            tool_budget: object.get("toolBudget").cloned(),
            session_file: object
                .get("sessionFile")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from),
            steer_inbox: None,
            skill_fallback_cwd: None,
            skill_primary_cwd: None,
        }
    }
}

/// `resolveForegroundTimeout` call-level values (aliases must agree).
fn resolve_call_timeout(object: &serde_json::Map<String, Value>) -> Option<u64> {
    let as_positive = |value: Option<&Value>| -> Option<u64> {
        match value {
            Some(Value::Number(n)) if n.is_u64() && n.as_u64().unwrap_or(0) > 0 => n.as_u64(),
            _ => None,
        }
    };
    let timeout = as_positive(object.get("timeoutMs"));
    let max_runtime = as_positive(object.get("maxRuntimeMs"));
    timeout.or(max_runtime)
}

/// Acceptance ledger per child run (FR-P1-09) — keyed by (run id, child
/// index) so parallel children never clobber each other; the details
/// assembly drains it.
pub static GATE_LEDGER: Mutex<BTreeMap<(String, u32), Value>> = Mutex::new(BTreeMap::new());

/// `getSubagentSessionRoot` fallback (extension/index.ts:224-231): no parent
/// session → a fresh temp directory. 0700 like upstream `mkdtempSync` — the
/// child session transcripts are private.
fn mkdtemp_session_root() -> PathBuf {
    let base = crate::paths::temp_dir().join("rpi-subagent-session-");
    let base = base.to_string_lossy().to_string();
    for _ in 0..32 {
        let candidate = format!("{base}{}", budget::random_run_id());
        let path = PathBuf::from(&candidate);
        if crate::paths::create_private_dir_all(&path).is_ok() && path.is_dir() {
            return path;
        }
    }
    PathBuf::from(format!("{base}{}", std::process::id()))
}

/// Everything the composite runners share for one delegation call.
#[derive(Clone)]
pub struct RunCtx {
    pub settings: SettingsPair,
    pub config: ExtensionConfig,
    pub base_cwd: PathBuf,
    pub parent_session_file: Option<PathBuf>,
    pub parent_model: Option<String>,
    pub registry: Vec<AvailableModel>,
    /// One id per delegation call; parallel/chain children share it so the
    /// `maxSubagentSpawnsPerRun` cap counts the composite, not per child.
    pub run_id: String,
    /// Top-level defaults children inherit when the child spec omits them.
    pub top_model: Option<String>,
    pub top_thinking: Option<String>,
    pub top_context: Option<ContextMode>,
    pub top_timeout_ms: Option<u64>,
    pub top_turn_budget: Option<Value>,
    pub top_tool_budget: Option<Value>,
    pub usage_budget: Option<Value>,
    pub artifacts_dir: Option<PathBuf>,
    pub session_root: PathBuf,
    /// Streaming frame sink (TE09 FR-A): foreground dispatches install the
    /// toolUpdate seam here; parallel/async paths leave it None (upstream
    /// wraps only the single and chain foreground flows).
    pub frame_sink: Option<crate::runner::foreground::StreamFrameSink>,
    /// Step activity projection (TE09 FR-C): async runs install a sink that
    /// mirrors currentTool/currentPath into the run status document.
    pub step_status: Option<crate::runner::foreground::StepStatusSink>,
}

impl RunCtx {
    /// Gather host-facing context once per delegation call.
    pub fn from_host(
        host: &dyn crate::HostContext,
        object: &serde_json::Map<String, Value>,
        config: ExtensionConfig,
    ) -> Self {
        let settings = crate::config::read_settings_pair(&host.cwd());
        let parent_session_file = host.parent_session_file(&settings);
        let effective_cwd = object
            .get("cwd")
            .and_then(Value::as_str)
            .map(crate::paths::expand_tilde_and_resolve)
            .unwrap_or_else(|| host.cwd());
        let run_id = budget::random_run_id();
        let session_root = match object.get("sessionDir").and_then(Value::as_str) {
            Some(dir) => crate::paths::expand_tilde_and_resolve(dir),
            None => match &config.default_session_dir {
                Some(dir) => crate::paths::expand_tilde_and_resolve(dir),
                None => parent_session_file
                    .as_ref()
                    .map(|file| {
                        file.parent()
                            .unwrap_or(&effective_cwd)
                            .join(file.file_stem().unwrap_or_default())
                    })
                    .unwrap_or_else(mkdtemp_session_root),
            },
        };
        let session_root =
            if object.get("sessionDir").is_some() || config.default_session_dir.is_some() {
                session_root
            } else {
                session_root.join(&run_id)
            };
        let artifacts_enabled = object.get("artifacts") != Some(&Value::Bool(false));
        let preference =
            crate::artifacts::ArtifactDirPreference::parse(Some(config.artifact_dir_preference()))
                .unwrap_or(crate::artifacts::ArtifactDirPreference::Project);
        let artifacts_dir = artifacts_enabled.then(|| {
            crate::artifacts::get_artifacts_dir(
                parent_session_file.as_deref(),
                Some(&effective_cwd),
                preference,
            )
        });
        Self {
            top_model: object
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            top_thinking: object
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            top_context: match object.get("context").and_then(Value::as_str) {
                Some("fork") => Some(ContextMode::Fork),
                Some("fresh") => Some(ContextMode::Fresh),
                Some(_) => Some(ContextMode::Fresh),
                None => None,
            },
            top_timeout_ms: resolve_call_timeout(object),
            top_turn_budget: object.get("turnBudget").cloned(),
            top_tool_budget: object.get("toolBudget").cloned(),
            usage_budget: object.get("usageBudget").cloned(),
            settings,
            config,
            base_cwd: effective_cwd,
            parent_session_file,
            parent_model: host.parent_model(),
            registry: host.scoped_models(),
            run_id,
            artifacts_dir,
            session_root,
            // Streaming sinks are per-dispatch concerns: the tool layer
            // installs them on the assembled ctx (None here keeps the
            // non-streaming default for every other constructor caller).
            frame_sink: None,
            step_status: None,
        }
    }

    /// Discover agents in both scopes for the effective cwd.
    pub fn discover(&self, scope: &str) -> Result<Vec<AgentConfig>, String> {
        discover::discover_agents(&self.base_cwd, scope, &self.settings, None)
    }
}

/// Outcome of one child launch.
pub struct ChildOutcome {
    pub agent_name: String,
    pub context: ContextMode,
    pub result: ForegroundRunResult,
    pub saved_output_path: Option<PathBuf>,
}

/// Launch one child to completion (foreground, with model fallback chain).
/// Synchronous entry for the single-run path (host dispatch thread).
pub fn run_child(
    spec: &ChildSpec,
    agent: &AgentConfig,
    ctx: &RunCtx,
    runtime: &PluginRuntime,
) -> Result<ChildOutcome, String> {
    runtime.block_on(run_child_async(spec, agent, ctx))
}

/// Async core shared by every execution path (single / parallel / chain /
/// background). Callers on the host dispatch thread wrap this with
/// `PluginRuntime::block_on`; parallel children call it directly inside one
/// runtime `block_on` — never nest `block_on` on the same runtime.
pub async fn run_child_async(
    spec: &ChildSpec,
    agent: &AgentConfig,
    ctx: &RunCtx,
) -> Result<ChildOutcome, String> {
    // Timeout chain: child override > top-level call > agent frontmatter >
    // config > 30min.
    let timeout = spec
        .timeout_ms
        .or(ctx.top_timeout_ms)
        .or(agent.default_timeout_ms)
        .or_else(|| ctx.config.resolve_default_timeout_ms())
        .or(Some(foreground::DEFAULT_FOREGROUND_TIMEOUT_MS));

    // Context policy: child > top-level > agent default (unknown → fresh).
    let context = spec
        .context
        .or(ctx.top_context)
        .or(agent.default_context)
        .unwrap_or(ContextMode::Fresh);

    let effective_cwd = spec.cwd.clone().unwrap_or_else(|| ctx.base_cwd.clone());

    // Fresh session dir / fork branch file are per child (executor 5929-5966:
    // explicit roots verbatim, derived roots get runId/<child>).
    // Resume overrides the session file regardless of context resolution.
    let (session_file, thinking_override) = if let Some(resume_file) = &spec.session_file {
        (Some(resume_file.clone()), None)
    } else if context == ContextMode::Fork {
        let branch_file = if spec.child_index == 0 {
            ctx.session_root.join("fork.jsonl")
        } else {
            ctx.session_root
                .join(format!("fork-{}.jsonl", spec.child_index))
        };
        match session_fork::create_fork_session(
            ctx.parent_session_file.as_deref(),
            &branch_file,
            &effective_cwd,
        ) {
            Ok(resolution) => (
                Some(resolution.session_file),
                resolution.thinking_override_off.then(|| "off".to_string()),
            ),
            Err(error) => return Err(error),
        }
    } else {
        (None, None)
    };
    let session_dir = (context != ContextMode::Fork).then(|| {
        if ctx.session_root == ctx.base_cwd {
            // Explicit roots are used verbatim; children still get distinct
            // session files via rpi's own session-dir behavior (same as P0).
            ctx.session_root.clone()
        } else {
            ctx.session_root.join(format!("run-{}", spec.child_index))
        }
    });

    // Model chain (FR-P1-05): child override > top-level > agent > parent,
    // fuzzy-resolved; fallback candidates built from the resolved primary.
    let parent_ref = ctx.parent_model.as_deref().and_then(|m| {
        let (provider, id) = m.split_once('/')?;
        Some((provider, id))
    });
    let registry_ref: Option<&[AvailableModel]> =
        (!ctx.registry.is_empty()).then_some(&ctx.registry[..]);
    let preferred_provider = parent_ref.map(|(provider, _)| provider);
    let scope = ctx.settings.model_scope.as_ref();
    let mut warn_sink = |violation: &model::ModelScopeViolation| {
        tracing::warn!(violation = %violation.message, "model scope violation");
    };
    let resolved = model::resolve_effective_subagent_model(
        spec.model.as_deref().or(ctx.top_model.as_deref()),
        agent.model.as_deref(),
        parent_ref,
        registry_ref,
        preferred_provider,
        scope,
        &mut warn_sink,
    )?;
    let candidates = model::build_model_candidates(
        resolved.as_deref(),
        &agent.fallback_models,
        registry_ref,
        preferred_provider,
        scope,
        &mut warn_sink,
    )?;
    let thinking = match thinking_override {
        Some(level) => Some(level),
        None => crate::launch::args::effective_thinking(
            agent,
            spec.thinking.as_deref().or(ctx.top_thinking.as_deref()),
        ),
    };

    // Skills: explicit step/child list > agent frontmatter; missing names
    // fail the run like upstream (`Skills not found: …`, execution.ts:1470).
    let skill_names: Vec<String> = match &spec.skills {
        Some(skills) => skills.clone(),
        None => agent.skills.clone(),
    };
    let skill_cwd = spec
        .skill_primary_cwd
        .clone()
        .unwrap_or_else(|| effective_cwd.clone());
    let (resolved_skills, missing_skills) = skills::resolve_skills_with_fallback(
        &skill_names,
        &skill_cwd,
        spec.skill_fallback_cwd.as_deref(),
    );
    if !missing_skills.is_empty() {
        return Err(format!("Skills not found: {}", missing_skills.join(", ")));
    }
    let mut system_prompt = agent.system_prompt.trim().to_string();
    // Budget instruction injection (FR-P1-09, turn-budget.ts L26-39).
    let budget_prompt = crate::p1::acceptance::build_budget_prompt(
        spec.turn_budget.as_ref().or(ctx.top_turn_budget.as_ref()),
        spec.tool_budget.as_ref().or(ctx.top_tool_budget.as_ref()),
    );
    if !budget_prompt.is_empty() {
        if system_prompt.is_empty() {
            system_prompt = budget_prompt.clone();
        } else {
            system_prompt = format!("{system_prompt}\n\n{budget_prompt}");
        }
    }

    if !resolved_skills.is_empty() {
        let injection = skills::build_skill_injection(&resolved_skills);
        if system_prompt.is_empty() {
            system_prompt = injection;
        } else {
            system_prompt = format!("{system_prompt}\n\n{injection}");
        }
    }
    // Per-agent memory injection (FR-P1-08, agent-memory.ts:193): the
    // MEMORY.md head rides the system prompt; write tools switch the block
    // to read-write.
    if let Some(memory) = &agent.memory {
        if let Some(dir) = memory.resolve_dir(&effective_cwd, &agent.name) {
            if let Some(text) = discover::read_agent_memory_file(&dir) {
                let writable = agent
                    .tools
                    .as_ref()
                    .map(|tools| {
                        tools
                            .iter()
                            .any(|t| matches!(t.as_str(), "edit" | "write" | "bash"))
                    })
                    .unwrap_or(false);
                let injection = discover::build_agent_memory_injection(&text, writable);
                if system_prompt.is_empty() {
                    system_prompt = injection;
                } else {
                    system_prompt = format!("{system_prompt}\n\n{injection}");
                }
            }
        }
    }
    // Project refinement overlay (FR-P1-08, execution.ts:1492).
    if let Some(overlay) = crate::actions::agent_refinement_overlay(&effective_cwd, &agent.name) {
        if system_prompt.is_empty() {
            system_prompt = overlay;
        } else {
            system_prompt = format!("{system_prompt}\n\n{overlay}");
        }
    }

    // Output path: child override > agent frontmatter.
    let output_path = match &spec.output {
        OutputOverride::Path(path) => Some(path.clone()),
        OutputOverride::Disabled => None,
        OutputOverride::Inherit => agent
            .output
            .as_deref()
            .map(crate::paths::expand_tilde_and_resolve),
    };

    // Spawn cap — one slot per child against the composite run id.
    let max_spawns = budget::resolve_max_spawns_per_run(
        ctx.config
            .max_subagent_spawns_per_run
            .as_ref()
            .and_then(Value::as_u64),
    );
    {
        let mut memory = crate::tool::FOREGROUND_RUN_MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let count = memory.spawns_by_run.entry(ctx.run_id.clone()).or_insert(0);
        *count += 1;
        if *count > max_spawns {
            return Err(format!(
                "Run fan-out budget exceeded: {max_spawns} subagent spawns per run."
            ));
        }
    }

    // Fanout authorization: only when the explicit allowlist names `subagent`.
    let fanout_authorized = agent
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t == "subagent"));
    let self_extension = crate::launch::binary::resolve_self_extension_path()
        .map(|p| p.to_string_lossy().to_string());
    if fanout_authorized && self_extension.is_none() {
        return Err(
            "This agent authorizes nested subagents (tools includes \"subagent\") but the subagents extension library path could not be resolved for child injection. Set RPI_SUBAGENT_EXTENSION_PATH to the installed librpi_ext_subagents shared library.".to_string(),
        );
    }

    let child_max_depth = budget::resolve_child_max_depth(
        budget::resolve_current_max_depth(
            ctx.config
                .max_subagent_depth
                .as_ref()
                .and_then(Value::as_u64),
        ),
        agent.max_subagent_depth,
    );

    // Supervisor channel (FR-P1-10): per-child channel dir env + the
    // intercomBridge tool/prompt application.
    let bridge_mode = ctx
        .config
        .intercom_bridge
        .as_ref()
        .and_then(|bridge| bridge.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("always")
        .to_string();
    let mut child_tools = agent.tools.clone();
    let mut bridge_prompt_source = system_prompt.clone();
    crate::p1::supervisor::apply_intercom_bridge(
        &bridge_mode,
        Some(context.as_str()),
        &mut child_tools,
        &mut bridge_prompt_source,
    );
    let supervisor_channel = if bridge_prompt_source != system_prompt || child_tools != agent.tools
    {
        let dir =
            crate::p1::supervisor::channel_dir(&ctx.run_id, &agent.name, spec.child_index as usize);
        crate::p1::supervisor::ensure_channel(&dir);
        Some(dir)
    } else {
        None
    };
    let system_prompt = bridge_prompt_source;
    let agent_tools = child_tools;

    // Fork task preamble (executor 4119-4122).
    let task_text = if context == ContextMode::Fork {
        session_fork::wrap_fork_task(&spec.task, None)
    } else {
        spec.task.clone()
    };

    let input = ForegroundRunInput {
        agent_name: agent.name.clone(),
        agent_system_prompt: system_prompt,
        agent_system_prompt_mode: agent.system_prompt_mode,
        agent_tools: agent_tools.clone(),
        agent_extensions: agent.extensions.clone(),
        agent_subagent_only_extensions: agent.subagent_only_extensions.clone(),
        agent_inherit_project_context: agent.inherit_project_context,
        agent_inherit_skills: agent.inherit_skills,
        task: task_text,
        task_delivery: None,
        cwd: effective_cwd.clone(),
        session_dir,
        session_file,
        model: candidates.first().cloned(),
        thinking,
        run_id: ctx.run_id.clone(),
        timeout_ms: timeout,
        child_index: spec.child_index,
        child_max_subagent_depth: child_max_depth,
        artifacts_dir: ctx.artifacts_dir.clone(),
        include_jsonl: ctx.config.include_jsonl(),
        include_transcript: true,
        parent_session_id: ctx.parent_session_file.as_ref().map(|p| {
            p.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        }),
        self_extension,
        fanout_authorized,
        resolved_skill_names: (!resolved_skills.is_empty()).then(|| {
            resolved_skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect()
        }),
        context_label: context.as_str().to_string(),
        steer_inbox: spec.steer_inbox.clone(),
        supervisor_channel,
        stream_sink: ctx.frame_sink.clone(),
        step_status: ctx.step_status.clone(),
    };

    let mut result = foreground::run_foreground_with_fallback(&input, &candidates).await;

    // Acceptance ledger (FR-P1-09): inferred level + parsed fenced report;
    // explicit gates run host-side and failing gates fail the run.
    {
        let (level, review_required) = crate::p1::acceptance::infer_level(
            &agent.name,
            agent.acceptance_role.as_deref(),
            &spec.task,
            false,
        );
        let report = crate::p1::acceptance::parse_acceptance_report(&result.final_output);
        if let Some(Err(message)) = &report {
            tracing::warn!(%message, "invalid acceptance report in child output");
        }
        // Evidence completeness for the inferred level
        // (`reportEvidenceStatus` shape): kinds the report omits are marked
        // missing (presence = the field exists and is non-empty).
        let mut evidence_status = serde_json::Map::new();
        let rank = crate::p1::acceptance::level_rank(&level).unwrap_or(0);
        for evidence_level_rank in [1u8, 2, 3] {
            if rank < evidence_level_rank {
                continue;
            }
            let evidence_level = match evidence_level_rank {
                1 => "attested",
                2 => "checked",
                _ => "verified",
            };
            for kind in crate::p1::acceptance::required_evidence_for_level(evidence_level) {
                let present = report.as_ref().map(|r| r.is_ok()).unwrap_or(false)
                    && report
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .and_then(|fields| fields.get(&kind.replace('-', "")))
                        .map(|value| {
                            value.as_array().is_some_and(|items| !items.is_empty())
                                || value.as_str().is_some_and(|s| !s.trim().is_empty())
                                || value.as_bool().unwrap_or(false)
                        })
                        .unwrap_or(false);
                evidence_status.insert(
                    kind.to_string(),
                    json!(if present { "satisfied" } else { "missing" }),
                );
            }
        }
        let mut ledger = json!({
            "level": level,
            "reviewRequired": review_required,
            "reportParsed": report.as_ref().map(|r| r.is_ok()).unwrap_or(false),
            "reportError": match report.as_ref() {
                Some(Err(message)) => Some(message.clone()),
                _ => None,
            },
            "evidenceStatus": evidence_status,
        });
        if let Some(gate) = &spec.gate {
            let explicit = true;
            // Memoized by workspace state (FR-P1-09 / acceptance.ts
            // runMemoizedVerifyCommand): same command + same tree = cached
            // verdict instead of a re-run.
            let (gate_passed, memoized) = crate::p1::acceptance::run_memoized_gate_command(
                gate,
                &effective_cwd,
                &ctx.run_id,
                ctx.artifacts_dir.as_deref(),
            );
            match gate_passed {
                Ok(true) => {
                    if memoized {
                        if let Some(object) = ledger.as_object_mut() {
                            object.insert("gateMemoized".to_string(), json!(true));
                        }
                    }
                }
                outcome => {
                    let message = match outcome {
                        Err(error) => format!("Acceptance gate error: {error}"),
                        _ => format!("Acceptance gate failed: {gate}"),
                    };
                    if explicit {
                        result.exit_code = result.exit_code.max(1);
                        result.error = Some(match result.error.take() {
                            Some(existing) => format!("{existing}; {message}"),
                            None => message,
                        });
                    }
                }
            }
        }
        // Ride the ledger for the details assembly (drained by tool.rs).
        GATE_LEDGER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((input.run_id.clone(), input.child_index), ledger.clone());
        // The acceptance ledger is part of the run record (design §3.5:
        // "_meta.json … P1 补 acceptance ledger") — merge it into the
        // per-child metadata file the foreground run just wrote.
        if let Some(artifacts_dir) = &ctx.artifacts_dir {
            let paths = crate::artifacts::get_artifact_paths(
                artifacts_dir,
                &input.run_id,
                &agent.name,
                Some(input.child_index),
            );
            if let Ok(raw) = std::fs::read_to_string(&paths.metadata_path) {
                if let Ok(mut metadata) = serde_json::from_str::<Value>(&raw) {
                    if let Some(target) = metadata.as_object_mut() {
                        target.insert("acceptance".to_string(), ledger);
                    }
                    let _ = crate::artifacts::write_metadata(&paths.metadata_path, &metadata);
                }
            }
        }
    }

    // Output file: write the full output to the declared path on success.
    let mut saved_output_path = None;
    if let Some(output_path) = &output_path {
        if result.exit_code == 0
            && !result.final_output.trim().is_empty()
            && crate::artifacts::write_artifact(output_path, &result.final_output).is_ok()
        {
            saved_output_path = Some(output_path.clone());
        }
    }

    Ok(ChildOutcome {
        agent_name: agent.name.clone(),
        context,
        result,
        saved_output_path,
    })
}
