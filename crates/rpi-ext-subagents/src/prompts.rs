//! Bundled prompt templates and the `/prompt-workflow` adapter (FR-P1-07).
//!
//! Port of pi-subagents `src/slash/prompt-workflows.ts` @ v0.48.0 (56f97234):
//! the five packaged templates (`prompts/*.md` from the pinned submodule via
//! `include_str!`) register as prompt-template commands; user templates in
//! `.rpi/prompts/` + `~/.rpi/agent/prompts/` with
//! `subagent:`/`model:`/`skill:`/`cwd:`/`fork:`/`fresh:`/`chain:` frontmatter
//! adapt to structured delegation calls (ADR-0018 W12: `chain:` maps to
//! `steps`, not workflowScript). Template and skill bodies are localized for
//! the structured entry points per ADR-0021 (no `workflowScript` teaching;
//! parity exemption recorded there). The bundled `pi-subagents` orchestration
//! skill ships to the user skill dir at install (parent sessions only —
//! children never resolve it, `SUBAGENT_ORCHESTRATION_SKILL`); a layout
//! version marker re-ships rewritten bodies over the pre-ADR-0021 byte-exact
//! install.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// `PROMPT_TEMPLATES` — bundled prompt names and bodies (`review-loop.md` is
/// localized for the structured entry points per ADR-0021; the rest are
/// byte-exact from the pinned submodule).
pub const BUNDLED_PROMPTS: [(&str, &str); 5] = [
    (
        "parallel-review",
        include_str!("../assets/prompts/parallel-review.md"),
    ),
    (
        "review-loop",
        include_str!("../assets/prompts/review-loop.md"),
    ),
    (
        "parallel-research",
        include_str!("../assets/prompts/parallel-research.md"),
    ),
    (
        "gather-context-and-clarify",
        include_str!("../assets/prompts/gather-context-and-clarify.md"),
    ),
    (
        "parallel-cleanup",
        include_str!("../assets/prompts/parallel-cleanup.md"),
    ),
];

/// The bundled orchestration skill body (SKILL.md + references).
pub const ORCHESTRATION_SKILL_SKILL_MD: &str =
    include_str!("../assets/skills/pi-subagents/SKILL.md");
pub const ORCHESTRATION_SKILL_REFERENCES: [(&str, &str); 4] = [
    (
        "constraints-and-recipes.md",
        include_str!("../assets/skills/pi-subagents/references/constraints-and-recipes.md"),
    ),
    (
        "execution-controls.md",
        include_str!("../assets/skills/pi-subagents/references/execution-controls.md"),
    ),
    (
        "management-authoring-rpc.md",
        include_str!("../assets/skills/pi-subagents/references/management-authoring-rpc.md"),
    ),
    (
        "prompting-and-roles.md",
        include_str!("../assets/skills/pi-subagents/references/prompting-and-roles.md"),
    ),
];

/// Layout version of the shipped orchestration skill (ADR-0021). v1 was the
/// byte-exact upstream copy with no version marker; v2 is the localized
/// rewrite (no `workflowScript` teaching). Bump this whenever the bundled
/// bodies change so existing installs are upgraded in place.
const ORCHESTRATION_SKILL_LAYOUT_VERSION: u32 = 2;

/// Install the orchestration skill into `<agentDir>/skills/pi-subagents/`
/// (upstream ships it with the package; rpi has no package skill path, so the
/// plugin materializes it). Idempotent per layout version: the marker file
/// `.rpi-layout-version` gates re-shipping — missing or older marker rewrites
/// the bodies (this covers the pre-marker v1 byte-exact install; user
/// customization should live in a renamed copy of the skill), equal or newer
/// marker leaves the directory untouched. Called at parent-mode install only.
pub fn install_orchestration_skill() {
    install_orchestration_skill_at(&crate::paths::get_agent_dir());
}

/// Directory-parameterized core for tests: the agent-dir env var is
/// process-global, so concurrent integration tests must not race through it.
pub fn install_orchestration_skill_at(agent_dir: &Path) {
    let skill_dir = agent_dir.join("skills").join("pi-subagents");
    let references = skill_dir.join("references");
    let version_path = skill_dir.join(".rpi-layout-version");
    let installed = std::fs::read_to_string(&version_path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    if matches!(installed, Some(v) if v >= ORCHESTRATION_SKILL_LAYOUT_VERSION) {
        return;
    }
    let _ = std::fs::create_dir_all(&references);
    let _ = std::fs::write(skill_dir.join("SKILL.md"), ORCHESTRATION_SKILL_SKILL_MD);
    for (name, body) in ORCHESTRATION_SKILL_REFERENCES {
        let _ = std::fs::write(references.join(name), body);
    }
    let _ = std::fs::write(
        &version_path,
        ORCHESTRATION_SKILL_LAYOUT_VERSION.to_string(),
    );
}

/// `getPromptDirectories` (prompt-workflows.ts:23-32) — project then user
/// template dirs; bundled names are reserved.
pub fn prompt_directories(cwd: &Path) -> Vec<PathBuf> {
    vec![
        cwd.join(".rpi").join("prompts"),
        crate::paths::get_agent_dir().join("prompts"),
    ]
}

/// Load a template by name: bundled first, then user/project dirs. The name
/// is user/model input joined into a path — path-shaped names are rejected
/// (C1).
pub fn load_template(name: &str, cwd: &Path) -> Option<String> {
    if let Some((_, body)) = BUNDLED_PROMPTS.iter().find(|(n, _)| *n == name) {
        return Some((*body).to_string());
    }
    crate::paths::ensure_safe_component(name, "Template name").ok()?;
    for dir in prompt_directories(cwd) {
        let path = dir.join(format!("{name}.md"));
        if let Ok(body) = std::fs::read_to_string(&path) {
            return Some(body);
        }
    }
    None
}

/// A parsed prompt template (prompt-workflows.ts L73-195).
#[derive(Debug, Default, Clone)]
pub struct PromptWorkflowSpec {
    pub description: Option<String>,
    /// `subagent:` target agent (missing/true → `delegate`, L73-77).
    pub subagent: String,
    pub model: Option<String>,
    pub skill: Option<Vec<String>>,
    pub cwd: Option<String>,
    /// `fork:`/`inheritContext: true` → fork; `fresh: true` → fresh.
    pub context: Option<&'static str>,
    /// `chain: "a -> b -> c"` → sequential agents (L193-195).
    pub chain: Vec<String>,
    /// Body (task template) after the frontmatter.
    pub body: String,
}

/// Parse a template file into an adapter spec.
pub fn parse_template(content: &str) -> PromptWorkflowSpec {
    let parsed = crate::agents::frontmatter::parse_frontmatter(content);
    let fm = &parsed.frontmatter;
    let subagent = fm
        .get("subagent")
        .map(|s| {
            if s.trim().is_empty() || s.trim() == "true" {
                "delegate".to_string()
            } else {
                s.trim().to_string()
            }
        })
        .unwrap_or_else(|| "delegate".to_string());
    let skill = fm.get("skill").map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "false")
            .map(str::to_string)
            .collect::<Vec<String>>()
    });
    let context = if fm.get("fresh").map(String::as_str) == Some("true") {
        Some("fresh")
    } else if fm.get("fork").map(String::as_str) == Some("true")
        || fm.get("inheritContext").map(String::as_str) == Some("true")
    {
        Some("fork")
    } else {
        None
    };
    let chain = fm
        .get("chain")
        .map(|raw| {
            raw.split("->")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    PromptWorkflowSpec {
        description: fm.get("description").cloned(),
        subagent,
        model: fm.get("model").cloned(),
        skill,
        cwd: fm.get("cwd").cloned(),
        context,
        chain,
        body: parsed.body,
    }
}

/// Argument substitution (L149-156): `$ARGUMENTS`, `$@`, `${1:-fallback}`,
/// `$1`..`$9`.
pub fn substitute_arguments(template: &str, args: &str) -> String {
    let positional: Vec<&str> = if args.trim().is_empty() {
        Vec::new()
    } else {
        args.split_whitespace().collect()
    };
    let mut out = template.replace("$ARGUMENTS", args).replace("$@", args);
    for index in 1..=9usize {
        let marker = format!("${index}");
        let fallback_marker = format!("${{{index}:-");
        if let Some(start) = out.find(&fallback_marker) {
            if let Some(end_offset) = out[start..].find('}') {
                let end = start + end_offset;
                let fallback = &out[start + fallback_marker.len()..end];
                let value = positional
                    .get(index - 1)
                    .copied()
                    .unwrap_or(fallback)
                    .to_string();
                out = format!("{}{}{}", &out[..start], value, &out[end + 1..]);
            }
        }
        if positional.get(index - 1).is_some() {
            out = out.replace(&marker, positional[index - 1]);
        }
    }
    out
}

/// Runtime flags (`--fork --fresh --bg --subagent <name>`, L158-190).
pub struct PromptWorkflowFlags {
    pub context: Option<&'static str>,
    pub background: bool,
    pub subagent_override: Option<String>,
}

pub fn parse_flags(args: &str) -> (PromptWorkflowFlags, String) {
    let mut flags = PromptWorkflowFlags {
        context: None,
        background: false,
        subagent_override: None,
    };
    let mut rest = Vec::new();
    let mut tokens = args.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        match token {
            "--fork" => flags.context = Some("fork"),
            "--fresh" => flags.context = Some("fresh"),
            "--bg" | "--async" => flags.background = true,
            "--subagent" => {
                if let Some(next) = tokens.next() {
                    let name = next.strip_suffix(|c: char| c.is_numeric()).unwrap_or(next);
                    flags.subagent_override = Some(name.to_string());
                }
            }
            other => rest.push(other),
        }
    }
    (flags, rest.join(" "))
}

/// Build the structured delegation params for an adapted template
/// (ADR-0018 W12): `chain:` → `steps`; otherwise a single delegation.
pub fn build_delegation_params(
    spec: &PromptWorkflowSpec,
    flags: &PromptWorkflowFlags,
    task: &str,
) -> Value {
    let agent = flags
        .subagent_override
        .clone()
        .unwrap_or_else(|| spec.subagent.clone());
    let context = flags.context.or(spec.context);
    let mut params = json!({
        "agent": agent,
        "task": task,
        "async": flags.background,
    });
    if let Some(model) = &spec.model {
        params["model"] = json!(model);
    }
    if let Some(cwd) = &spec.cwd {
        params["cwd"] = json!(cwd);
    }
    if let Some(context) = context {
        params["context"] = json!(context);
    }
    if !spec.chain.is_empty() {
        // Sequential chain of agents; the task template rides every step
        // (`{task}` interpolation carries the user args).
        let steps: Vec<Value> = spec
            .chain
            .iter()
            .map(|agent| {
                let mut step = json!({ "agent": agent, "task": task });
                if let Some(model) = &spec.model {
                    step["model"] = json!(model);
                }
                if let Some(skills) = &spec.skill {
                    step["skill"] = json!(skills.join(","));
                }
                step
            })
            .collect();
        params = json!({
            "steps": steps,
            "async": flags.background,
        });
        return params;
    }
    if let Some(skills) = &spec.skill {
        params["skill"] = json!(skills.join(","));
    }
    params
}

/// Handle a prompt-template command: bundled names expand the template into
/// the chat (prompt text for the parent model to act on, upstream
/// prompt-template semantics); `/prompt-workflow <name> [args] [flags]`
/// adapts a template into a structured delegation call.
pub fn handle_prompt_command(
    name: &str,
    args: &str,
    host: &dyn crate::HostContext,
    settings: &crate::config::SettingsPair,
    config: &crate::config::ExtensionConfig,
    runtime: &crate::PluginRuntime,
) -> Option<Value> {
    if name == "prompt-workflow" {
        let (template_name, rest) = match args.split_once(char::is_whitespace) {
            Some((template_name, rest)) => (template_name.trim(), rest),
            None => (args.trim(), ""),
        };
        if template_name.is_empty() {
            return Some(json!({
                "content": [{ "type": "text", "text":
                    "Usage: /prompt-workflow <template> [args] [--fork|--fresh|--bg|--subagent <agent>]" }]
            }));
        }
        let Some(body) = load_template(template_name, &host.cwd()) else {
            return Some(json!({
                "content": [{ "type": "text", "text": format!("Unknown prompt template '{template_name}'.") }],
                "isError": true,
            }));
        };
        let spec = parse_template(&body);
        let (flags, positional) = parse_flags(rest);
        let task = substitute_arguments(&spec.body, &positional);
        let params = build_delegation_params(&spec, &flags, &task);
        return Some(crate::tool::execute_subagent_tool(
            &params, host, settings, config, runtime, None,
        ));
    }
    // Bundled prompt shortcut: expand the template into the chat.
    if BUNDLED_PROMPTS.iter().any(|(n, _)| *n == name) {
        let expanded = substitute_arguments(&load_template(name, &host.cwd())?, args);
        return Some(json!({
            "content": [{ "type": "text", "text": expanded }]
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_templates_parse() {
        for (name, body) in BUNDLED_PROMPTS {
            let spec = parse_template(body);
            assert!(
                spec.description.is_some(),
                "{name} must carry a description"
            );
            assert!(!spec.body.trim().is_empty(), "{name} must have a body");
        }
    }

    #[test]
    fn template_frontmatter_adapts() {
        let template = "---\ndescription: Take a screenshot\nmodel: m1\nsubagent: browser\ncwd: /tmp\nskill: a, b\nfork: true\n---\nUse $@ to shoot";
        let spec = parse_template(template);
        assert_eq!(spec.subagent, "browser");
        assert_eq!(spec.model.as_deref(), Some("m1"));
        assert_eq!(spec.cwd.as_deref(), Some("/tmp"));
        assert_eq!(spec.skill, Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(spec.context, Some("fork"));
        // subagent:true → delegate default.
        let spec = parse_template("---\ndescription: d\nsubagent: true\n---\nbody");
        assert_eq!(spec.subagent, "delegate");
        // chain splits on ->.
        let spec =
            parse_template("---\ndescription: d\nchain: scout -> worker -> reviewer\n---\nb");
        assert_eq!(spec.chain, vec!["scout", "worker", "reviewer"]);
    }

    #[test]
    fn argument_substitution_forms() {
        assert_eq!(substitute_arguments("go $@ now", "a b"), "go a b now");
        assert_eq!(substitute_arguments("go $ARGUMENTS", "x"), "go x");
        assert_eq!(substitute_arguments("${1:-def}", ""), "def");
        assert_eq!(substitute_arguments("$1 and $2", "a b"), "a and b");
        // Missing positional markers stay literal.
        assert_eq!(substitute_arguments("$3", "a"), "$3");
    }

    #[test]
    fn chain_builds_steps() {
        let spec = parse_template("---\ndescription: d\nchain: scout -> worker\n---\ndo {task}");
        let flags = PromptWorkflowFlags {
            context: None,
            background: true,
            subagent_override: None,
        };
        let params = build_delegation_params(&spec, &flags, "the task");
        assert!(params["steps"].is_array());
        assert_eq!(params["steps"].as_array().unwrap().len(), 2);
        assert_eq!(params["steps"][0]["agent"], "scout");
        assert_eq!(params["async"], Value::Bool(true));
    }

    #[test]
    fn flags_parse() {
        let (flags, rest) = parse_flags("--fork --bg --subagent reviewer2 the rest");
        assert_eq!(flags.context, Some("fork"));
        assert!(flags.background);
        assert_eq!(flags.subagent_override.as_deref(), Some("reviewer"));
        assert_eq!(rest, "the rest");
    }
}
