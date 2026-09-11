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
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::agents::discover::{self, AgentConfig, ContextMode};
use crate::agents::skills;
use crate::config::{ExtensionConfig, SettingsPair};
use crate::launch::model::{self, AvailableModel};
use crate::runner::budget;
use crate::runner::foreground::{self, ForegroundRunInput, ForegroundRunResult};
use crate::{session_fork, ParentSession, PluginRuntime};

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
    /// #1303 `context: "profile"`: use the agent's declared
    /// `defaultContext` instead of the call-level default — escapes a
    /// top-level `context` inherited by every task/step.
    pub context_profile: bool,
    pub cwd: Option<PathBuf>,
    pub output: OutputOverride,
    /// #1305 call/step-level `outputMode`: inline | file-only (defaults
    /// through to the agent's own `outputMode`, then "inline").
    pub output_mode: Option<String>,
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
                Some("profile") => None,
                Some(_) => Some(ContextMode::Fresh),
                None => None,
            },
            context_profile: object.get("context").and_then(Value::as_str) == Some("profile"),
            cwd: object
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(crate::paths::expand_tilde_and_resolve),
            output,
            output_mode: match object.get("outputMode").and_then(Value::as_str) {
                Some("inline") | Some("file-only") => object
                    .get("outputMode")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            },
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
    /// Authoritative parent session identity (V13-02, ADR-0022); `file` is
    /// `None` only for a not-yet-persisted in-memory parent.
    pub parent_session: Option<ParentSession>,
    pub parent_session_file: Option<PathBuf>,
    /// Parent session id (authoritative `ctx.sessionFile.id`, fallback =
    /// uuidv7 tail of the fallback file stem).
    pub parent_session_id: Option<String>,
    pub parent_model: Option<String>,
    pub registry: Vec<AvailableModel>,
    /// Host tool names from `getAllTools` (R7.1.4.3 / #2034): `Ok` = the
    /// authoritative host set (possibly empty), `Err(reason)` = the host
    /// call failed (the pre-spawn gate fails closed on that). Captured once
    /// per delegation call alongside the model registry.
    pub host_builtin_tool_names: Result<Vec<String>, String>,
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
    /// Abort probe for the dispatch this context serves (see
    /// [`crate::runner::foreground::AbortProbe`]); `None` when there is no
    /// host channel or the context outlives its dispatch (async runs).
    pub abort_probe: Option<crate::runner::foreground::AbortProbe>,
}

impl RunCtx {
    /// Gather host-facing context once per delegation call.
    pub fn from_host(
        host: &dyn crate::HostContext,
        object: &serde_json::Map<String, Value>,
        config: ExtensionConfig,
    ) -> Self {
        let settings = crate::config::read_settings_pair(&host.cwd());
        let parent_session = host.parent_session(&settings);
        let parent_session_file = parent_session
            .as_ref()
            .and_then(|session| session.file.clone());
        let parent_session_id = parent_session.as_ref().map(|session| session.id.clone());
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
            parent_session,
            parent_session_file,
            parent_session_id,
            parent_model: host.parent_model(),
            registry: host.scoped_models(),
            host_builtin_tool_names: host.host_tool_names(),
            run_id,
            artifacts_dir,
            session_root,
            // Streaming sinks are per-dispatch concerns: the tool layer
            // installs them on the assembled ctx (None here keeps the
            // non-streaming default for every other constructor caller).
            frame_sink: None,
            step_status: None,
            abort_probe: None,
        }
    }

    /// Discover agents in both scopes for the effective cwd.
    pub fn discover(&self, scope: &str) -> Result<Vec<AgentConfig>, String> {
        discover::discover_agents(&self.base_cwd, scope, &self.settings, None)
    }
}

/// `deriveChildSessionName` (#1615, child-session-name.ts @ 0fc0eebb):
/// `agent: <task excerpt>` where the excerpt is `previewDisplayText(task,
/// 60)`; the whole name caps at 80. `None` only when both inputs are empty.
fn derive_child_session_name(agent: &str, task: &str) -> Option<String> {
    let agent = agent.trim();
    let excerpt_source = task.trim();
    let excerpt = if excerpt_source.is_empty() {
        String::new()
    } else {
        crate::runner::display::preview_display_text(excerpt_source, 60)
    };
    let base = if !agent.is_empty() && !excerpt.is_empty() {
        format!("{agent}: {excerpt}")
    } else if !agent.is_empty() {
        agent.to_string()
    } else {
        excerpt
    };
    if base.is_empty() {
        return None;
    }
    Some(crate::runner::display::preview_display_text(&base, 80))
}

/// Outcome of one child launch.
pub struct ChildOutcome {
    pub agent_name: String,
    pub context: ContextMode,
    pub result: ForegroundRunResult,
    pub saved_output_path: Option<PathBuf>,
    /// #1615 child display name (`deriveChildSessionName`): threaded into
    /// the run status steps and result payloads.
    pub session_name: Option<String>,
    /// Effective output mode (#1305): "inline" | "file-only".
    pub output_mode: &'static str,
}

/// Outcome of the fork attempt (ADR-0026 decision 1, TE18 FR-G): either a
/// usable fork branch, or the structured reason the run degraded to fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkOutcome {
    Forked(PathBuf, Option<String>),
    Degraded(String),
}

/// Attempt a fork branch for one child, mapping every unusable path to a
/// degradation reason (never `Err`): the in-memory parent (V13-02 FR-A R2
/// wording), the missing parent session, and `create_fork_session` failures
/// (upstream #1137 keeps explicit `context: "fork"` fail-fast for the
/// in-memory/missing-parent cases; ADR-0026 broadened degradation to every
/// path — the e2e scenario-2 expectation change is registered under G2).
pub fn try_fork_session(ctx: &RunCtx, branch_file: &Path, effective_cwd: &Path) -> ForkOutcome {
    if let Some(parent) = &ctx.parent_session {
        if parent.file.is_none() {
            return ForkOutcome::Degraded(
                "parent session is in-memory and not yet persisted to disk".to_string(),
            );
        }
    }
    match session_fork::create_fork_session(
        ctx.parent_session_file.as_deref(),
        branch_file,
        effective_cwd,
    ) {
        Ok(resolution) => ForkOutcome::Forked(
            resolution.session_file,
            resolution.thinking_override_off.then(|| "off".to_string()),
        ),
        Err(error) => {
            ForkOutcome::Degraded(format!("failed to create forked subagent session: {error}"))
        }
    }
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
/// Fanout authorization predicate (#1587, child-tool-plan.ts:329-333):
/// the explicit `tools` allowlist naming `subagent`, OR
/// `allowNestedSubagents: true` while `excludeTools` does not name it.
/// Shared by the child-argv check below and the dispatch-time budget
/// preflight in `tool.rs` (TE18 independent tracking item 1 closure,
/// v0.1.4 M7: the misconfiguration must fail BEFORE the session spawn
/// budget is reserved, not after).
pub fn agent_authorizes_nested_fanout(agent: &crate::agents::discover::AgentConfig) -> bool {
    agent
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t == "subagent"))
        || (agent.allow_nested_subagents == Some(true)
            && !agent.exclude_tools.iter().any(|t| t == "subagent"))
}

/// The fanout self-extension precondition error (single source for the
/// dispatch-time preflight and the in-depth child-argv check).
pub fn self_extension_missing_error() -> String {
    "This agent authorizes nested subagents (tools includes \"subagent\") but the subagents extension library path could not be resolved for child injection. Set RPI_SUBAGENT_EXTENSION_PATH to the installed librpi_ext_subagents shared library.".to_string()
}

/// Dispatch-time preflight (TE18 independent tracking item 1 closure,
/// v0.1.4 M7): when the self-extension path cannot be resolved and any
/// referenced agent authorizes nested fanout, reject the run BEFORE the
/// session spawn budget is reserved — previously each attempt burned
/// budget before failing in the child-argv builder, so a missing
/// `RPI_SUBAGENT_EXTENSION_PATH` eventually surfaced as budget exhaustion
/// instead of the actionable config error. Unknown agent names are left to
/// their own existing failure paths.
pub fn preflight_self_extension(
    agent_names: &[String],
    agents: &[crate::agents::discover::AgentConfig],
    self_extension: Option<&std::path::Path>,
) -> Option<String> {
    if self_extension.is_some() {
        return None;
    }
    for name in agent_names {
        if let Ok(Some(agent)) = crate::agents::discover::resolve_agent_name(agents, name) {
            if agent_authorizes_nested_fanout(agent) {
                return Some(self_extension_missing_error());
            }
        }
    }
    None
}

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
    let mut context = if spec.context_profile {
        // #1303 `context: "profile"`: the agent's declared `defaultContext`
        // wins over the call-level default (top_context) — the per-step
        // escape from a global context default.
        agent.default_context.unwrap_or(ContextMode::Fresh)
    } else {
        spec.context
            .or(ctx.top_context)
            .or(agent.default_context)
            .unwrap_or(ContextMode::Fresh)
    };

    let effective_cwd = spec.cwd.clone().unwrap_or_else(|| ctx.base_cwd.clone());

    // Fresh session dir / fork branch file are per child (executor 5929-5966:
    // explicit roots verbatim, derived roots get runId/<child>).
    // Resume overrides the session file regardless of context resolution.
    let (session_file, thinking_override) = if let Some(resume_file) = &spec.session_file {
        (Some(resume_file.clone()), None)
    } else if context == ContextMode::Fork {
        // ADR-0026 decision 1 (TE18 FR-G, upstream #1137): every fork-unusable
        // path degrades to `fresh` with a structured warning instead of
        // failing the run — covering the in-memory parent (V13-02 FR-A R2),
        // the missing parent session, and `create_fork_session` failures.
        // The effective mode is reflected in the result's `context` field.
        let branch_file = if spec.child_index == 0 {
            ctx.session_root.join("fork.jsonl")
        } else {
            ctx.session_root
                .join(format!("fork-{}.jsonl", spec.child_index))
        };
        match try_fork_session(ctx, &branch_file, &effective_cwd) {
            ForkOutcome::Forked(file, thinking) => (Some(file), thinking),
            ForkOutcome::Degraded(reason) => {
                tracing::warn!(
                    reason = %reason,
                    "fork context unavailable; degraded subagent to fresh (ADR-0026)"
                );
                context = ContextMode::Fresh;
                (None, None)
            }
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
    // R7.1.4.4 (TE18 FR-D): an empty `ctx.scopedModels` registry is NOT
    // "no usable models" — model strings pass through verbatim and fuzzy
    // resolution is explicitly skipped. The branch must stay visible in
    // diagnostics so a later regression to silent passthrough is caught.
    if let Some(diagnostic) = model::registry_unavailable_diagnostic(
        spec.model.is_some()
            || ctx.top_model.is_some()
            || agent.model.is_some()
            || !agent.fallback_models.is_empty(),
    ) {
        tracing::warn!(diagnostic, "subagent model resolution degraded");
    }
    // Model origin (upstream `resolveModelOrigin`, model-fallback.ts:437-450
    // @ 0fc0eebb): explicit call param > parent-inherited > agent-configured.
    // Decides where the required (fail-closed, #1093) check applies.
    let explicit_model = spec.model.as_deref().or(ctx.top_model.as_deref());
    let model_origin =
        model::resolve_model_origin(explicit_model, agent.model.as_deref(), parent_ref);
    // #1393 (execution.ts:1798): the agent's provider (settings
    // `subagents.defaultProvider` fill or builtin override) outranks the
    // parent session's provider as the preferred resolution provider.
    let preferred_provider = agent
        .model_provider
        .as_deref()
        .or_else(|| parent_ref.map(|(provider, _)| provider));
    let scope = ctx.settings.model_scope.as_ref();
    let mut warn_sink = |violation: &model::ModelScopeViolation| {
        tracing::warn!(violation = %violation.message, "model scope violation");
    };
    let resolved = model::resolve_effective_subagent_model(
        explicit_model,
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
        Some(&agent.name),
        parent_ref,
        model_origin,
        &mut warn_sink,
    )?;
    let thinking = match thinking_override {
        Some(level) => Some(level),
        None => crate::launch::args::effective_thinking(
            agent,
            spec.thinking.as_deref().or(ctx.top_thinking.as_deref()),
        ),
    };

    // #1397 `subagents.maxThinking` (thinking-ceiling.ts @ 0fc0eebb): the
    // settings ceiling (project wins) intersected with the inherited
    // ceiling (env — a child launched under a tighter ancestor keeps it,
    // the rpi mapping of upstream's launch-contract `thinkingCeiling`).
    // Fail-closed: an explicit requested level above the ceiling aborts
    // the launch before the spawn (`assertThinkingWithinCeiling`).
    let thinking_ceiling = model::intersect_thinking_ceilings([
        ctx.settings.max_thinking.as_deref(),
        std::env::var(crate::launch::args::SUBAGENT_THINKING_CEILING_ENV)
            .ok()
            .as_deref(),
    ]);
    if let Some(ceiling) = thinking_ceiling {
        if let Some(requested) =
            model::effective_requested_thinking(resolved.as_deref(), thinking.as_deref())
        {
            model::assert_thinking_within_ceiling(&requested, ceiling, &agent.name, &ctx.run_id)?;
        }
    }

    // #1615 `deriveChildSessionName` (child-session-name.ts): display-only
    // `agent: task excerpt` (excerpt ≤60 chars, total cap 80) threaded into
    // the child env (the child's plugin init calls `setSessionName`) and the
    // result/status payloads.
    let session_name = derive_child_session_name(&agent.name, &spec.task);

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

    // Effective output mode (#1305, subagent-executor.ts:3260 @ 0fc0eebb):
    // call/step param > agent frontmatter/builtin override > "inline".
    // `file-only` requires a writable output path
    // (`validateFileOnlyOutputMode`).
    let output_mode: &'static str = match spec.output_mode.as_deref() {
        Some("file-only") => "file-only",
        Some("inline") => "inline",
        Some(_) => "inline",
        None => match agent.output_mode.as_deref() {
            Some("file-only") => "file-only",
            _ => "inline",
        },
    };
    if output_mode == "file-only" && output_path.is_none() {
        return Err(format!(
            "Single run ({}) sets outputMode: \"file-only\" but does not configure an output file. Set output to a path or use outputMode: \"inline\".",
            agent.name
        ));
    }

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

    // Fanout authorization (#1587): the explicit tools allowlist naming
    // `subagent`, OR `allowNestedSubagents: true` while `excludeTools`
    // does not name it (child-tool-plan.ts:329-333 — the flag authorizes
    // nested fanout without replacing inherited tools/extensions).
    let fanout_authorized = agent_authorizes_nested_fanout(agent);
    let self_extension = crate::launch::binary::resolve_self_extension_path()
        .map(|p| p.to_string_lossy().to_string());
    if fanout_authorized && self_extension.is_none() {
        return Err(self_extension_missing_error());
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

    // Pre-spawn tool-face gate (R7.1.4.3 / #2034, TE18 FR-C): the declared
    // builtin allowlist must be covered by the host's tool set before the
    // child starts — rpi fails closed (ADR-0017 wording) where upstream
    // silently drops the missing names, so a child never starts with an
    // allowlist it cannot satisfy. Runs only when an allowlist is declared;
    // excluded names (post-`--exclude-tools`) are not requirements.
    if let Some(allowlist) = agent_tools.as_ref() {
        if let Err(error) = crate::diagnostic::check_host_tool_face(
            allowlist,
            &agent.exclude_tools,
            ctx.host_builtin_tool_names
                .as_deref()
                .map_err(|e| e.as_str()),
            &agent.name,
        ) {
            // A pre-spawn rejection never launched a process: give the
            // spawn-budget slot back so misconfigured agents cannot starve
            // valid siblings in the same composite run (review round 1,
            // observation 3).
            let mut memory = crate::tool::FOREGROUND_RUN_MEMORY
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(count) = memory.spawns_by_run.get_mut(&ctx.run_id) {
                *count = count.saturating_sub(1);
            }
            return Err(error);
        }
    }

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
        agent_exclude_tools: agent.exclude_tools.clone(),
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
        parent_session_id: ctx.parent_session_id.clone(),
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
        thinking_ceiling: thinking_ceiling.map(str::to_string),
        session_name: session_name.clone(),
        supervisor_channel,
        stream_sink: ctx.frame_sink.clone(),
        step_status: ctx.step_status.clone(),
        abort_probe: ctx.abort_probe.clone(),
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
                    // TE17 R7.1.7.1: child metadata is an auxiliary artifact
                    // — an exhausted write logs instead of dropping silently.
                    if let Err(error) =
                        crate::artifacts::write_metadata(&paths.metadata_path, &metadata)
                    {
                        tracing::warn!(
                            path = %paths.metadata_path.display(),
                            error = %error,
                            "child metadata write failed after retrying"
                        );
                    }
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
    // #1305 `file-only`: the saved file is authoritative — the returned
    // content becomes the saved-output reference instead of the inline
    // text (`formatSavedOutputReference`, single-output.ts:160-172).
    if output_mode == "file-only" {
        if let Some(saved) = &saved_output_path {
            let bytes = result.final_output.len();
            let lines = result.final_output.lines().count().max(1);
            let size = if bytes < 1024 {
                format!("{bytes} B")
            } else if bytes < 1024 * 1024 {
                format!("{:.1} KB", bytes as f64 / 1024.0)
            } else {
                format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
            };
            let reference = format!(
                "Output saved to: {} ({size}, {lines} {}). Read this file if needed.",
                saved.to_string_lossy(),
                if lines == 1 { "line" } else { "lines" }
            );
            result.final_output = reference;
        }
    }

    Ok(ChildOutcome {
        agent_name: agent.name.clone(),
        context,
        result,
        saved_output_path,
        session_name,
        output_mode,
    })
}

/// TE18 FR-G (ADR-0026 decision 1, upstream #1137): every fork-unusable path
/// degrades to fresh with a structured reason — never `Err`.
#[cfg(test)]
mod te18_fork_tests {
    use super::*;

    fn ctx_with(parent: Option<ParentSession>, parent_file: Option<PathBuf>) -> RunCtx {
        RunCtx {
            settings: Default::default(),
            config: Default::default(),
            base_cwd: PathBuf::from("/tmp"),
            parent_session: parent,
            parent_session_file: parent_file,
            parent_session_id: None,
            parent_model: None,
            registry: Vec::new(),
            host_builtin_tool_names: Ok(Vec::new()),
            run_id: "te18fork1".to_string(),
            top_model: None,
            top_thinking: None,
            top_context: None,
            top_timeout_ms: None,
            top_turn_budget: None,
            top_tool_budget: None,
            usage_budget: None,
            artifacts_dir: None,
            session_root: std::env::temp_dir(),
            frame_sink: None,
            step_status: None,
            abort_probe: None,
        }
    }

    #[test]
    fn in_memory_parent_degrades_with_reason() {
        // V13-02 FR-A R2 case: parent has an id but no persisted file.
        let ctx = ctx_with(
            Some(ParentSession {
                file: None,
                id: "abc".to_string(),
            }),
            None,
        );
        match try_fork_session(&ctx, Path::new("/tmp/branch.jsonl"), Path::new("/tmp")) {
            ForkOutcome::Degraded(reason) => {
                assert!(
                    reason.contains("in-memory") && reason.contains("not yet persisted to disk"),
                    "{reason}"
                );
            }
            other => panic!("expected degradation, got {other:?}"),
        }
    }

    #[test]
    fn missing_parent_session_degrades_with_reason() {
        // No parent session at all: create_fork_session reports the missing
        // persisted parent; the outcome is still a degradation, never Err.
        let ctx = ctx_with(None, None);
        match try_fork_session(&ctx, Path::new("/tmp/branch.jsonl"), Path::new("/tmp")) {
            ForkOutcome::Degraded(reason) => {
                assert!(
                    reason.contains("requires a persisted parent session"),
                    "{reason}"
                );
            }
            other => panic!("expected degradation, got {other:?}"),
        }
    }

    #[test]
    fn create_fork_session_failure_degrades_with_reason() {
        // A persisted-looking parent whose file does not exist: the fork
        // builder errors, the launch degrades instead of failing.
        let missing = std::env::temp_dir().join("rpi-sub-te18-missing-parent.jsonl");
        let ctx = ctx_with(
            Some(ParentSession {
                file: Some(missing.clone()),
                id: "abc".to_string(),
            }),
            Some(missing),
        );
        match try_fork_session(&ctx, Path::new("/tmp/branch.jsonl"), Path::new("/tmp")) {
            ForkOutcome::Degraded(reason) => {
                assert!(
                    reason.contains("failed to create forked subagent session"),
                    "{reason}"
                );
            }
            other => panic!("expected degradation, got {other:?}"),
        }
    }

    #[test]
    fn persisted_parent_still_forks() {
        // Regression guard: a real persisted parent keeps forking (no
        // degradation) — write a minimal session file with a leaf.
        let dir = std::env::temp_dir().join(format!("rpi-sub-te18-fork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let parent = dir.join("parent.jsonl");
        std::fs::write(
            &parent,
            concat!(
                r#"{"type":"session","version":3,"id":"p1","timestamp":"2026-09-09T00:00:00.000Z","cwd":"/tmp"}"#,
                "\n",
                r#"{"type":"message","role":"user","content":[{"type":"text","text":"hi"}],"timestamp":"2026-09-09T00:00:01.000Z"}"#,
                "\n"
            ),
        )
        .unwrap();
        let ctx = ctx_with(
            Some(ParentSession {
                file: Some(parent.clone()),
                id: "p1".to_string(),
            }),
            Some(parent),
        );
        match try_fork_session(&ctx, &dir.join("branch.jsonl"), Path::new("/tmp")) {
            ForkOutcome::Forked(file, _) => assert!(file.ends_with("branch.jsonl")),
            other => panic!("expected a fork, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Review round 1, observation 3: a pre-spawn gate rejection must not consume
/// the composite run's spawn budget (the slot is returned before the Err).
#[cfg(test)]
mod te18_gate_budget_tests {
    use super::*;
    use crate::agents::discover::AgentSource;

    /// TE18 independent tracking item 1 (v0.1.4 M7): the fanout
    /// self-extension misconfiguration must be rejected at dispatch time —
    /// before the session spawn budget is reserved. With the resolved path
    /// injected (None = misconfigured install), a fanout-authorized agent
    /// yields the actionable config error; a plain agent passes the
    /// preflight untouched.
    #[test]
    fn preflight_self_extension_rejects_fanout_before_budget() {
        let fanout_agent = crate::agents::discover::agent_from_content(
            "---\nname: fanout\ndescription: d\ntools:\n  - subagent\n---\nbody",
            std::path::Path::new("/x/fanout.md"),
            AgentSource::User,
        )
        .expect("agent parses")
        .expect("agent present");
        let plain_agent = crate::agents::discover::agent_from_content(
            "---\nname: plain\ndescription: d\ntools:\n  - web_search\n---\nbody",
            std::path::Path::new("/x/plain.md"),
            AgentSource::User,
        )
        .expect("agent parses")
        .expect("agent present");
        let agents = vec![fanout_agent, plain_agent];

        assert!(agent_authorizes_nested_fanout(&agents[0]));
        assert!(!agent_authorizes_nested_fanout(&agents[1]));

        let names_all = vec!["plain".to_string(), "fanout".to_string()];
        let names_plain = vec!["plain".to_string()];
        let resolved = Some(std::path::Path::new("/opt/lib/librpi_ext_subagents.so"));

        // Missing path: the fanout agent is rejected with the actionable
        // config error; the plain agent passes untouched.
        let error = preflight_self_extension(&names_all, &agents, None)
            .expect("fanout misconfiguration rejected before budget");
        assert_eq!(error, self_extension_missing_error());
        assert!(preflight_self_extension(&names_plain, &agents, None).is_none());

        // Path resolved: nothing is rejected either way.
        assert!(preflight_self_extension(&names_all, &agents, resolved).is_none());
    }

    #[test]
    fn gate_rejection_returns_the_spawn_budget_slot() {
        let runtime = crate::PluginRuntime::new().expect("plugin runtime");
        let ctx = RunCtx {
            settings: Default::default(),
            config: Default::default(),
            base_cwd: std::env::temp_dir(),
            parent_session: None,
            parent_session_file: None,
            parent_session_id: None,
            parent_model: None,
            registry: Vec::new(),
            // Empty host set: every declared builtin tool is missing.
            host_builtin_tool_names: Ok(Vec::new()),
            run_id: "te18budget".to_string(),
            top_model: None,
            top_thinking: None,
            top_context: None,
            top_timeout_ms: Some(30_000),
            top_turn_budget: None,
            top_tool_budget: None,
            usage_budget: None,
            artifacts_dir: None,
            session_root: std::env::temp_dir(),
            frame_sink: None,
            step_status: None,
            abort_probe: None,
        };
        let agent = AgentConfig {
            name: "gater".to_string(),
            local_name: "gater".to_string(),
            package_name: None,
            description: "d".to_string(),
            aliases: None,
            tools: Some(vec!["web_search".to_string()]),
            exclude_tools: Vec::new(),
            mcp_direct_tools: Vec::new(),
            model: None,
            fallback_models: Vec::new(),
            thinking: crate::agents::discover::ThinkingSpec::Unset,
            system_prompt_mode: "replace",
            inherit_project_context: true,
            inherit_skills: false,
            default_context: None,
            default_async: None,
            default_timeout_ms: None,
            system_prompt: String::new(),
            source: AgentSource::User,
            file_path: PathBuf::from("/tmp/gater.md"),
            skills: Vec::new(),
            extensions: None,
            subagent_only_extensions: None,
            output: None,
            output_mode: None,
            advertise: None,
            allow_nested_subagents: None,
            model_provider: None,
            default_reads: Vec::new(),
            default_progress: false,
            max_subagent_depth: None,
            disabled: None,
            acceptance_role: None,
            memory: None,
            frontmatter_fields: Default::default(),
        };
        let spec = ChildSpec {
            task: "needs web_search".to_string(),
            child_index: 0,
            ..Default::default()
        };
        let Err(error) = run_child(&spec, &agent, &ctx, &runtime) else {
            panic!("gate rejects the missing tool");
        };
        assert!(
            error.contains("requested unavailable child tools: web_search"),
            "{error}"
        );
        // The budget slot was returned — the counter is back to zero.
        let memory = crate::tool::FOREGROUND_RUN_MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            memory.spawns_by_run.get("te18budget"),
            Some(&0),
            "gate rejection must not consume the spawn budget"
        );
    }
    // ---- TE19: session name + output mode --------------------------------

    /// #1615 `deriveChildSessionName`: `agent: excerpt` (60-char excerpt,
    /// 80-char total); label preferred, empty inputs → None.
    #[test]
    fn child_session_name_shape() {
        assert_eq!(
            derive_child_session_name("scout", "do the thing").as_deref(),
            Some("scout: do the thing")
        );
        assert_eq!(
            derive_child_session_name("scout", "").as_deref(),
            Some("scout")
        );
        assert_eq!(
            derive_child_session_name("", "task only").as_deref(),
            Some("task only")
        );
        assert_eq!(derive_child_session_name("", ""), None);
        // Long task: excerpt capped at 60 chars.
        let long_task = "t".repeat(200);
        let name = derive_child_session_name("scout", &long_task).unwrap();
        assert!(name.chars().count() <= 80, "{}", name.chars().count());
        assert!(name.starts_with("scout: "), "{name}");
        // Total cap: a very long agent name alone still caps at 80.
        let name = derive_child_session_name(&"a".repeat(120), "").unwrap();
        assert!(name.chars().count() <= 80, "{}", name.chars().count());
    }

    /// #1305: call-level outputMode parse (inline|file-only; anything else
    /// falls through to the agent's own setting downstream).
    #[test]
    fn output_mode_parse_from_params() {
        let mut object = serde_json::Map::new();
        object.insert("agent".into(), json!("scout"));
        object.insert("task".into(), json!("t"));
        object.insert("outputMode".into(), json!("file-only"));
        let spec = ChildSpec::from_params(&object);
        assert_eq!(spec.output_mode.as_deref(), Some("file-only"));
        object.insert("outputMode".into(), json!("inline"));
        let spec = ChildSpec::from_params(&object);
        assert_eq!(spec.output_mode.as_deref(), Some("inline"));
        object.insert("outputMode".into(), json!("boxed"));
        let spec = ChildSpec::from_params(&object);
        assert_eq!(spec.output_mode, None, "unknown values defer downstream");
    }

    /// #1303: `context: "profile"` parses as the profile marker (context
    /// cleared; the launch path resolves the agent's declared default).
    #[test]
    fn context_profile_parse_from_params() {
        let mut object = serde_json::Map::new();
        object.insert("agent".into(), json!("scout"));
        object.insert("task".into(), json!("t"));
        object.insert("context".into(), json!("profile"));
        let spec = ChildSpec::from_params(&object);
        assert!(spec.context.is_none());
        assert!(spec.context_profile);
    }
}
