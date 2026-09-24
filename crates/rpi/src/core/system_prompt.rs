//! Port of `packages/coding-agent/src/core/system-prompt.ts`
//! @ pi 0.84.1+ (4181f66), plus the context-file and system-prompt-source
//! parts of `packages/coding-agent/src/core/resource-loader.ts`:
//! `resolvePromptInput` (:50-65), `loadContextFileFromDir` (:67-86),
//! `loadProjectContextFiles` (:88-123) with
//! `findShadowedContextFile` (:100-116, commit cced6a21d),
//! `discoverSystemPromptFile` (:969-981) and
//! `discoverAppendSystemPromptFile` (:983-995).
//!
//! Context files: per directory the first hit of `AGENTS.override.md`,
//! `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD` (in that priority order)
//! wins. Loading order is
//! the global agent dir first, then the full ancestor chain from the
//! filesystem root down to cwd (NOT bounded by the git repo root),
//! deduplicated by path, and independent of project trust. Loaded files are
//! injected at the end of the system prompt inside `<project_context>` /
//! `<project_instructions>` blocks (byte-exact format, see
//! [`build_system_prompt`]).
//!
//! Intentional differences:
//! - `formatSkillsForPrompt` (`skills.ts`) is not ported yet — the skills
//!   section arrives pre-formatted via
//!   [`BuildSystemPromptOptions::skills_xml`] (a non-empty string plays the
//!   role of upstream `skills.length > 0`). The "read tool available" gate
//!   stays here, exactly as upstream.
//! - Upstream embeds pi's bundled README/docs/examples paths
//!   (`getReadmePath`/`getDocsPath`/`getExamplesPath`, config.ts:427-439,
//!   anchored at pi's package dir). rpi has no bundled package docs yet, so
//!   the three paths arrive via [`BuildSystemPromptOptions::doc_paths`];
//!   `None` omits the whole "Pi documentation" paragraph. The surrounding
//!   assembly (tools list, guidelines, context injection, cwd line) is
//!   byte-faithful.
//! - `cwd`/`agent_dir` inputs are resolved with `resolve_path` inside each
//!   filesystem-touching function; upstream resolves them once in the
//!   `DefaultResourceLoader` constructor (resource-loader.ts:218-219).
//! - Read failures are logged with `tracing::warn!` instead of
//!   `console.error(chalk.yellow(...))`.
//! - `.rpi` rename per ADR-0001 (`CONFIG_DIR_NAME`, `SYSTEM.md` /
//!   `APPEND_SYSTEM.md` live under it project-side).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config;
use crate::tools::path_utils::resolve_path;

/// A loaded context file (`{ path, content }`, resource-loader.ts:67).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextFile {
    /// `join(dir, filename)` — the directory the file was found in, joined
    /// with the winning candidate name.
    pub path: PathBuf,
    /// UTF-8 file content.
    pub content: String,
}

/// Candidate file names, in priority order (resource-loader.ts:71, commit 8ecf8a988).
const CONTEXT_FILE_CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------
// Context files (resource-loader.ts:67-123)
// ---------------------------------------------------------------------------

/// `loadContextFileFromDir` (resource-loader.ts:67-86): return the first
/// readable candidate of `AGENTS.override.md` > `AGENTS.md` > `AGENTS.MD` >
/// `CLAUDE.md` > `CLAUDE.MD` in `dir`. A candidate that exists but is not a
/// file is skipped silently; a stat/read failure logs a warning and falls
/// through to the next candidate (upstream `try/catch` around `statSync` +
/// `readFileSync`).
pub fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    for filename in CONTEXT_FILE_CANDIDATES {
        let file_path = dir.join(filename);
        if !file_path.exists() {
            continue;
        }
        match std::fs::metadata(&file_path) {
            Ok(stats) if !stats.is_file() => continue,
            Ok(_) => match std::fs::read_to_string(&file_path) {
                Ok(content) => {
                    return Some(ContextFile {
                        path: file_path,
                        content,
                    });
                }
                Err(error) => {
                    tracing::warn!("Warning: Could not read {}: {}", file_path.display(), error);
                }
            },
            Err(error) => {
                tracing::warn!("Warning: Could not read {}: {}", file_path.display(), error);
            }
        }
    }
    None
}

/// `findShadowedContextFile` (resource-loader.ts:100-116, commit cced6a21d):
/// in a **nested** linked worktree (`git worktree add ./feat`), the main
/// checkout lives in an ancestor directory and may carry the same context
/// file. Without dedup the file is loaded twice — once from the worktree
/// root (ancestor walk) and once from the main checkout (also an ancestor).
///
/// Returns the canonicalized path of the shadowed file in the main repo
/// root, if any. The caller skips a context file whose canonical path
/// matches this value.
///
/// Returned canonicalized (realpath), because `git worktree add` writes the
/// `.git` file's `gitdir:` target in realpath form while cwd may still be
/// symlinked (macOS `/tmp` -> `/private/tmp`).
fn find_shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let git_paths = crate::core::git_paths::find_git_paths(cwd)?;
    let common_git_dir = canonicalize_path(&git_paths.common_git_dir);
    let worktree_root = canonicalize_path(&git_paths.repo_dir);
    let main_repo_root = common_git_dir.parent()?.to_path_buf();

    // False for an ordinary repo, where the two are the same dir, and for a
    // sibling worktree (`git worktree add ../feat`), whose main repo is not
    // an ancestor.
    if !worktree_root.starts_with(&main_repo_root) {
        return None;
    }
    let separator_check = {
        let mut prefix = main_repo_root.clone();
        prefix.push(""); // adds trailing separator
        worktree_root.starts_with(&main_repo_root)
            && worktree_root != main_repo_root
            && worktree_root.starts_with(prefix)
    };
    if !separator_check {
        return None;
    }

    // dirname of the common git dir is the main worktree root only when that
    // dir is itself checked out from the same repo. In a bare layout
    // (`proj/.bare` + `proj/main`) it is just the directory holding `.bare`,
    // which tracks nothing; a submodule's gitdir has no `commondir`, so it
    // lands under `.git/modules`.
    let main_git_path = canonicalize_path(&main_repo_root.join(".git"));
    if main_git_path != common_git_dir {
        return None;
    }

    let worktree_context = load_context_file_from_dir(&worktree_root)?;
    let filename = worktree_context.path.file_name()?;
    Some(main_repo_root.join(filename))
}

/// `canonicalizePath` (resource-loader.ts:95-98): `fs.realpathSync`.
fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `loadProjectContextFiles` (resource-loader.ts:118-156, commit cced6a21d).
///
/// Order: the global agent-dir context file first, then the ancestor chain
/// from the filesystem root down to `cwd` (root side first, cwd last —
/// upstream `unshift`). Paths are deduplicated; loading happens regardless
/// of project trust. In a nested linked worktree, a context file that
/// shadows the worktree's own from the main checkout is skipped
/// (see [`find_shadowed_context_file`]).
///
/// `include_global: false` skips ONLY the global agent-dir segment while the
/// ancestor (project/repo) chain still loads (ADR-0026 decision 2, TE18
/// FR-H): subagent children opt out of the operator's global `AGENTS.md`
/// via the `RPI_NO_GLOBAL_CONTEXT=1` env switch, matching upstream #1560
/// where in-process children default `inheritGlobalContext` to false.
pub fn load_project_context_files(
    cwd: &Path,
    agent_dir: &Path,
    include_global: bool,
) -> Vec<ContextFile> {
    let resolved_cwd = resolve_path(&cwd.to_string_lossy(), &process_cwd());
    let resolved_agent_dir = resolve_path(&agent_dir.to_string_lossy(), &process_cwd());

    let mut context_files: Vec<ContextFile> = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    if include_global {
        if let Some(global_context) = load_context_file_from_dir(&resolved_agent_dir) {
            seen_paths.insert(global_context.path.clone());
            context_files.push(global_context);
        }
    }

    let mut ancestor_context_files: Vec<ContextFile> = Vec::new();
    let shadowed_context_file = find_shadowed_context_file(&resolved_cwd);
    let mut current_dir = resolved_cwd;

    loop {
        let context_file = load_context_file_from_dir(&current_dir);
        let is_shadowed = match (&context_file, &shadowed_context_file) {
            (Some(cf), Some(shadowed)) => canonicalize_path(&cf.path) == *shadowed,
            _ => false,
        };
        if let Some(context_file) = context_file {
            if !is_shadowed && !seen_paths.contains(&context_file.path) {
                seen_paths.insert(context_file.path.clone());
                // unshift: ancestors end up root-first, cwd last.
                ancestor_context_files.insert(0, context_file);
            }
        }

        // dirname(currentDir) === currentDir → filesystem root.
        match current_dir.parent() {
            Some(parent_dir) => current_dir = parent_dir.to_path_buf(),
            None => break,
        }
    }

    context_files.extend(ancestor_context_files);
    context_files
}

// ---------------------------------------------------------------------------
// System prompt sources (resource-loader.ts:50-65, 969-995)
// ---------------------------------------------------------------------------

/// `discoverSystemPromptFile` (resource-loader.ts:969-981): the project
/// `SYSTEM.md` (`{cwd}/.rpi/SYSTEM.md`) wins but requires project trust;
/// otherwise the global `{agentDir}/SYSTEM.md` is used when present.
pub fn discover_system_prompt_file(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
) -> Option<PathBuf> {
    discover_prompt_file(
        cwd,
        agent_dir,
        project_trusted,
        config::SYSTEM_PROMPT_FILE_NAME,
    )
}

/// `discoverAppendSystemPromptFile` (resource-loader.ts:983-995): same
/// trust gate and priority as [`discover_system_prompt_file`], for
/// `APPEND_SYSTEM.md`.
pub fn discover_append_system_prompt_file(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
) -> Option<PathBuf> {
    discover_prompt_file(
        cwd,
        agent_dir,
        project_trusted,
        config::APPEND_SYSTEM_PROMPT_FILE_NAME,
    )
}

fn discover_prompt_file(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    file_name: &str,
) -> Option<PathBuf> {
    let resolved_cwd = resolve_path(&cwd.to_string_lossy(), &process_cwd());
    let resolved_agent_dir = resolve_path(&agent_dir.to_string_lossy(), &process_cwd());

    let project_path = config::get_project_config_dir(&resolved_cwd).join(file_name);
    if project_trusted && project_path.exists() {
        return Some(project_path);
    }

    let global_path = resolved_agent_dir.join(file_name);
    if global_path.exists() {
        return Some(global_path);
    }

    None
}

/// `resolvePromptInput` (resource-loader.ts:50-65): a `--system-prompt` /
/// `--append-system-prompt` value that names an existing file is read from
/// disk; anything else (missing file, unreadable file, plain text) is used
/// as inline text. Empty/`None` input yields `None` (upstream `!input`).
pub fn resolve_prompt_input(input: Option<&str>, description: &str) -> Option<String> {
    let input = input.filter(|s| !s.is_empty())?;

    if Path::new(input).exists() {
        match std::fs::read_to_string(input) {
            Ok(content) => return Some(content),
            Err(error) => {
                tracing::warn!("Warning: Could not read {description} file {input}: {error}");
                return Some(input.to_string());
            }
        }
    }

    Some(input.to_string())
}

// ---------------------------------------------------------------------------
// System prompt assembly (system-prompt.ts @ #9548: section architecture)
// ---------------------------------------------------------------------------

/// Bundled documentation paths for the default system prompt's "Rpi
/// documentation" section (upstream: `getReadmePath` / `getDocsPath` /
/// `getExamplesPath`, config.ts:427-439). Supplied by the caller because
/// rpi has no bundled package docs dir yet (see module header).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocPaths {
    /// Main documentation (README.md).
    pub readme_path: String,
    /// Additional docs directory.
    pub docs_path: String,
    /// Examples directory.
    pub examples_path: String,
}

/// `BuildSystemPromptOptions` (system-prompt.ts:8-30 @ #9548), with `skills`
/// replaced by the pre-formatted [`Self::skills_xml`] slot and the bundled
/// doc paths made explicit via [`Self::doc_paths`] (see module header).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BuildSystemPromptOptions {
    /// Custom system prompt (replaces the default preamble and the
    /// tools/rules/docs sections).
    pub custom_prompt: Option<String>,
    /// Exact full prompt replacement set by a `before_agent_start` handler
    /// (#9548): the forced text lives in the system message `content` with
    /// no sections.
    pub force_system_prompt: Option<String>,
    /// Tools to include in the prompt. Default: `["read", "bash", "edit",
    /// "write"]` (system-prompt.ts:17).
    pub selected_tools: Option<Vec<String>>,
    /// Optional one-line tool snippets keyed by tool name.
    pub tool_snippets: Option<HashMap<String, String>>,
    /// Guideline bullets contributed by each tool, keyed by tool name
    /// (`toolGuidelines`, system-prompt.ts:19). Per-tool lines render in
    /// selected-tool order inside the rules section, so a mid-run loadout
    /// change re-derives the guidelines for the CURRENT tools.
    pub tool_guidelines: Option<HashMap<String, Vec<String>>>,
    /// Additional guideline bullets appended after the per-tool lines
    /// (`promptGuidelines`).
    pub prompt_guidelines: Vec<String>,
    /// Text appended from user configuration before project context, skills,
    /// and cwd.
    pub append_system_prompt: Option<String>,
    /// Additional XML-wrapped prompt sections keyed by tag name. The JSON
    /// object keeps insertion order (`preserve_order`), matching upstream's
    /// `Record<string, string>` enumeration order (system-prompt.ts:171-176).
    pub sections: Option<serde_json::Map<String, serde_json::Value>>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Pre-loaded context files.
    pub context_files: Vec<ContextFile>,
    /// Pre-formatted skills section (output of the skills module's
    /// `format_skills_for_prompt`). A non-empty value plays the role of
    /// upstream `skills.length > 0`; rendered trimmed.
    pub skills_xml: Option<String>,
    /// Bundled documentation paths for the default prompt; `None` omits the
    /// "Rpi documentation" section (rpi difference, see module header).
    pub doc_paths: Option<DocPaths>,
}

/// Ordered prompt sections, `preamble` first (untagged). These become
/// `SystemMessage.sections` in the transcript (#9548).
pub type SystemPromptSections = Vec<(String, String)>;

/// `SYSTEM_PROMPT_SECTION_NAME` (system-prompt.ts:46-47).
fn is_valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && name != "preamble"
}

/// `renderProjectContext` (system-prompt.ts:84-91): the leading line plus
/// one `<project_instructions>` block per file, joined by blank lines. No
/// outer wrapper post-#9548 — the `<project_context>` tag comes from the
/// section wrapping itself.
fn render_project_context(context_files: &[ContextFile]) -> String {
    let mut parts = vec!["Project-specific instructions and guidelines:".to_owned()];
    for file in context_files {
        parts.push(format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            file.path.display(),
            file.content
        ));
    }
    parts.join("\n\n")
}

/// `buildRules` (system-prompt.ts:81-118): tool-driven exploration
/// Guidelines bullets: tool-driven exploration rules, then the per-tool
/// guidelines in selected-tool order (`toolGuidelines[name]`), then the
/// custom bullets (`promptGuidelines`), then the two always-on closers
/// (system-prompt.ts:111-116 @ #9548). Deduplicated, order-preserving.
fn build_rules(
    selected_tools: &[&str],
    tool_guidelines: Option<&HashMap<String, Vec<String>>>,
    prompt_guidelines: &[String],
) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add_rule = |rule: &str| {
        let normalized = rule.trim();
        if !normalized.is_empty() && seen.insert(normalized.to_owned()) {
            rules.push(normalized.to_owned());
        }
    };

    let has_bash = selected_tools.contains(&"bash");
    let has_powershell = selected_tools.contains(&"powershell");
    let has_grep = selected_tools.contains(&"grep");
    let has_find = selected_tools.contains(&"find");
    let has_ls = selected_tools.contains(&"ls");

    if (has_bash || has_powershell) && !has_grep && !has_find && !has_ls {
        if has_bash && has_powershell {
            add_rule("Use bash or PowerShell for file operations like listing, searching, and finding files");
        } else if has_powershell {
            add_rule(
                "Use PowerShell for file operations like listing, searching, and finding files",
            );
        } else {
            add_rule("Use bash for file operations like ls, rg, find");
        }
    }

    for name in selected_tools {
        if let Some(lines) = tool_guidelines.and_then(|guidelines| guidelines.get(*name)) {
            for line in lines {
                add_rule(line);
            }
        }
    }
    for guideline in prompt_guidelines {
        add_rule(guideline);
    }
    add_rule("Be concise in your responses");
    add_rule("Show file paths clearly when working with files");
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `buildSystemPromptSections` (system-prompt.ts:120-179 @ #9548): build the
/// ordered, independently replaceable sections of the structured system
/// prompt. `preamble` is untagged; every other section is wrapped in a tag
/// of the same name so the model can match later updates to it.
pub fn build_system_prompt_sections(options: &BuildSystemPromptOptions) -> SystemPromptSections {
    let default_tools: Vec<&str> = vec!["read", "bash", "edit", "write"];
    let tools: Vec<&str> = match &options.selected_tools {
        Some(selected) => selected.iter().map(String::as_str).collect(),
        None => default_tools,
    };

    // Upstream throws on an invalid section name inside the handler chain
    // (runner catches per handler); rpi validates at the extension mutation
    // boundary instead, so the builder only warns and skips defensively.
    // Non-string values are outside the `Record<string, string>` contract
    // and skip with the same warning.
    let valid_custom_sections: Vec<(&String, &str)> = options
        .sections
        .as_ref()
        .map(|sections| {
            sections
                .iter()
                .filter_map(|(name, value)| Some((name, value.as_str()?)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| {
            let valid = is_valid_section_name(name);
            if !valid {
                tracing::warn!("Invalid system prompt section name: {name}");
            }
            valid
        })
        .collect();

    // `if (customPrompt)` — an empty custom prompt falls through to the
    // default branch (empty string is falsy upstream).
    let custom_prompt = options.custom_prompt.as_deref().filter(|s| !s.is_empty());

    let mut prompt_sections: Vec<(String, String)> = Vec::new();
    match custom_prompt {
        Some(custom_prompt) => {
            prompt_sections.push(("preamble".to_owned(), custom_prompt.to_owned()));
        }
        None => {
            prompt_sections.push((
                "preamble".to_owned(),
                "You are an expert coding assistant operating inside rpi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files."
                    .to_owned(),
            ));
            let snippet = |name: &str| -> Option<&str> {
                options
                    .tool_snippets
                    .as_ref()
                    .and_then(|snippets| snippets.get(name))
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
            };
            let visible_tools: Vec<&str> = tools
                .iter()
                .copied()
                .filter(|name| snippet(name).is_some())
                .collect();
            let tools_list = if visible_tools.is_empty() {
                "(none)".to_owned()
            } else {
                visible_tools
                    .iter()
                    .map(|name| format!("- {name}: {}", snippet(name).unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            prompt_sections.push((
                "tools".to_owned(),
                format!(
                    "{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."
                ),
            ));
            prompt_sections.push((
                "rules".to_owned(),
                build_rules(
                    &tools,
                    options.tool_guidelines.as_ref(),
                    &options.prompt_guidelines,
                ),
            ));
            if let Some(doc_paths) = &options.doc_paths {
                prompt_sections.push((
                    "docs".to_owned(),
                    format!(
                        "Rpi documentation (read only when the user asks about rpi itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: {}\n- Additional docs: {}\n- Examples: {} (extensions, custom tools, SDK)\n- When reading rpi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), rpi packages (docs/packages.md), environment variables (docs/environment-variables.md)\n- When working on rpi topics, read the docs and examples, and follow .md cross-references before implementing\n- Always read rpi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
                        doc_paths.readme_path, doc_paths.docs_path, doc_paths.examples_path
                    ),
                ));
            }
        }
    }

    if let Some(append) = options
        .append_system_prompt
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        prompt_sections.push(("addendum".to_owned(), append.to_owned()));
    }
    if !options.context_files.is_empty() {
        prompt_sections.push((
            "project_context".to_owned(),
            render_project_context(&options.context_files),
        ));
    }
    // Skills section only when a skill-file-read tool is available
    // (system-prompt.ts:160-167, 1d6dbf9e3); the gate checks the TRIMMED
    // text like upstream (`formatSkillsForPrompt(...).trim()` non-empty).
    let skill_file_read_tool = ["read", "bash"].iter().find(|tool| tools.contains(tool));
    if let (Some(_), Some(skills_xml)) = (
        skill_file_read_tool,
        options
            .skills_xml
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        prompt_sections.push(("skills".to_owned(), skills_xml.to_owned()));
    }
    prompt_sections.push((
        "cwd".to_owned(),
        options.cwd.to_string_lossy().replace('\\', "/"),
    ));
    for (name, content) in valid_custom_sections {
        if content.is_empty() {
            continue;
        }
        // Upstream object assignment replaces an existing key in place.
        match prompt_sections
            .iter_mut()
            .find(|(existing, _)| existing == name)
        {
            Some(existing) => existing.1 = content.to_owned(),
            None => prompt_sections.push((name.clone(), content.to_owned())),
        }
    }

    // Wrap every non-preamble section in its tag; preamble stays raw.
    prompt_sections
        .into_iter()
        .map(|(name, content)| {
            if name == "preamble" {
                (name, content)
            } else {
                (name.clone(), format!("<{name}>\n{content}\n</{name}>"))
            }
        })
        .collect()
}

/// `buildSystemPromptState` (system-prompt.ts:186-191): the complete prompt
/// state for a build. A forced prompt is opaque and lives in `content` with
/// no sections; otherwise `content` is empty and the structured sections
/// carry the prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemPromptState {
    pub content: String,
    pub sections: Option<SystemPromptSections>,
}

pub fn build_system_prompt_state(options: &BuildSystemPromptOptions) -> SystemPromptState {
    if let Some(forced) = options.force_system_prompt.as_deref() {
        return SystemPromptState {
            content: forced.to_owned(),
            sections: None,
        };
    }
    SystemPromptState {
        content: String::new(),
        sections: Some(build_system_prompt_sections(options)),
    }
}

/// `buildSystemPrompt` (system-prompt.ts:193-195 @ #9548): build the system
/// prompt text, rendered exactly as the transcript's system message replays
/// it — content followed by sections, joined by blank lines.
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    let state = build_system_prompt_state(options);
    let mut parts = vec![state.content];
    if let Some(sections) = &state.sections {
        parts.extend(sections.iter().map(|(_, text)| text.clone()));
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `diffSystemPromptSections` (system-prompt.ts:197-210): diff the sections
/// the model currently has (replayed from the transcript, so never null)
/// against the desired ones. Returns a `SystemMessage.sections` patch, or
/// `None` when nothing changed. `None` values are removal markers.
pub fn diff_system_prompt_sections(
    previous: &SystemPromptSections,
    current: &SystemPromptSections,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut patch = serde_json::Map::new();
    for (name, text) in current {
        let previous_text = previous
            .iter()
            .find(|(previous_name, _)| previous_name == name)
            .map(|(_, text)| text.as_str());
        if previous_text != Some(text.as_str()) {
            patch.insert(name.clone(), serde_json::Value::String(text.clone()));
        }
    }
    for (name, _) in previous {
        if !current.iter().any(|(current_name, _)| current_name == name) {
            patch.insert(name.clone(), serde_json::Value::Null);
        }
    }
    (!patch.is_empty()).then_some(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_file(path: &str, content: &str) -> ContextFile {
        ContextFile {
            path: PathBuf::from(path),
            content: content.to_string(),
        }
    }

    // ---- injection format -------------------------------------------------

    #[test]
    fn project_context_block_byte_exact() {
        let options = BuildSystemPromptOptions {
            custom_prompt: Some("CUSTOM".to_string()),
            cwd: PathBuf::from("/repo"),
            context_files: vec![
                context_file("/agent/AGENTS.md", "global rules"),
                context_file("/repo/AGENTS.md", "project rules"),
            ],
            ..Default::default()
        };
        let prompt = build_system_prompt(&options);
        // #9548 section architecture: preamble + tagged sections joined by
        // blank lines (upstream system-prompt.test.ts "maps appended
        // instructions and project context to stable sections").
        assert_eq!(
            prompt,
            "CUSTOM\n\n<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/agent/AGENTS.md\">\nglobal rules\n</project_instructions>\n\n<project_instructions path=\"/repo/AGENTS.md\">\nproject rules\n</project_instructions>\n</project_context>\n\n<cwd>\n/repo\n</cwd>"
        );
    }

    #[test]
    fn empty_context_files_emit_no_block() {
        let options = BuildSystemPromptOptions {
            custom_prompt: Some("CUSTOM".to_string()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        assert_eq!(
            build_system_prompt(&options),
            "CUSTOM\n\n<cwd>\n/repo\n</cwd>"
        );
    }

    #[test]
    fn append_section_placement() {
        // Custom branch: custom + append + context + cwd.
        let options = BuildSystemPromptOptions {
            custom_prompt: Some("CUSTOM".to_string()),
            append_system_prompt: Some("EXTRA".to_string()),
            context_files: vec![context_file("/repo/AGENTS.md", "rules")],
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        let prompt = build_system_prompt(&options);
        // #9548: append rides the `<addendum>` section.
        assert!(prompt.starts_with("CUSTOM\n\n<addendum>\nEXTRA\n</addendum>\n\n<project_context>"));
        // Empty append string is falsy upstream.
        let options = BuildSystemPromptOptions {
            append_system_prompt: Some(String::new()),
            ..options
        };
        assert!(!build_system_prompt(&options).contains("\n\n\n"));
    }

    // ---- default branch assembly -------------------------------------------

    #[test]
    fn default_prompt_tools_and_guidelines() {
        let snippets: HashMap<String, String> = [
            ("read".to_string(), "Read a file".to_string()),
            ("bash".to_string(), "Run a command".to_string()),
        ]
        .into_iter()
        .collect();
        let options = BuildSystemPromptOptions {
            tool_snippets: Some(snippets),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        let prompt = build_system_prompt(&options);
        // Only snippet-bearing tools are listed (edit/write have none) —
        // inside the `<tools>` section (#9548).
        assert!(prompt.contains(
            "<tools>\n- read: Read a file\n- bash: Run a command\n\nIn addition to the tools above"
        ));
        // Default tools include bash but not grep/find/ls → exploration
        // guideline, inside `<rules>`.
        assert!(prompt.contains("<rules>\n- Use bash for file operations like ls, rg, find\n"));
        assert!(prompt.contains(
            "- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>"
        ));
        assert!(prompt.ends_with("<cwd>\n/repo\n</cwd>"));
    }

    #[test]
    fn default_prompt_no_visible_tools() {
        let options = BuildSystemPromptOptions {
            tool_snippets: Some(HashMap::new()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        assert!(build_system_prompt(&options).contains("<tools>\n(none)\n"));
    }

    #[test]
    fn guidelines_selection_and_dedup() {
        // grep present → no bash exploration guideline.
        let options = BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".to_string(), "grep".to_string()]),
            prompt_guidelines: vec![
                "  Extra rule  ".to_string(),
                "Extra rule".to_string(),                   // dup after trim
                "Be concise in your responses".to_string(), // dup of built-in
            ],
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        let prompt = build_system_prompt(&options);
        assert!(!prompt.contains("Use bash for file operations"));
        // #9548: rules ride the `<rules>` section.
        let rules = prompt
            .split("<rules>\n")
            .nth(1)
            .and_then(|tail| tail.split("\n</rules>").next())
            .expect("rules section");
        assert_eq!(rules.matches("- Extra rule").count(), 1);
        assert_eq!(rules.matches("Be concise in your responses").count(), 1);
    }

    #[test]
    fn skills_gate_requires_read_tool() {
        let base = BuildSystemPromptOptions {
            custom_prompt: Some("CUSTOM".to_string()),
            skills_xml: Some("<skills>XML</skills>".to_string()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        // #9548: the section wrapper adds the outer <skills> tag.
        assert!(build_system_prompt(&base).contains("<skills>\n<skills>XML</skills>\n</skills>"));
        // bash also keeps skills discoverable (1d6dbf9e3, #8552).
        let options = BuildSystemPromptOptions {
            selected_tools: Some(vec!["bash".to_string()]),
            ..base.clone()
        };
        assert!(build_system_prompt(&options).contains("<skills>XML</skills>"));
        // Neither read nor bash selected → skills dropped (custom branch).
        let options = BuildSystemPromptOptions {
            selected_tools: Some(vec!["edit".to_string(), "write".to_string()]),
            ..base.clone()
        };
        assert!(!build_system_prompt(&options).contains("<skills>XML</skills>"));
        // Default branch: same gate via the default tools (read included).
        let options = BuildSystemPromptOptions {
            custom_prompt: None,
            ..base
        };
        assert!(build_system_prompt(&options).contains("<skills>XML</skills>"));
        // Empty skills string = no skills.
        let options = BuildSystemPromptOptions {
            skills_xml: Some(String::new()),
            ..options
        };
        assert!(!build_system_prompt(&options).contains("<skills>"));
    }

    #[test]
    fn doc_paths_paragraph_optional() {
        let options = BuildSystemPromptOptions {
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        assert!(!build_system_prompt(&options).contains("Rpi documentation"));
        let options = BuildSystemPromptOptions {
            doc_paths: Some(DocPaths {
                readme_path: "/pkg/README.md".to_string(),
                docs_path: "/pkg/docs".to_string(),
                examples_path: "/pkg/examples".to_string(),
            }),
            ..options
        };
        let prompt = build_system_prompt(&options);
        assert!(prompt.contains("Rpi documentation (read only when the user asks about rpi itself"));
        assert!(prompt.contains("- Main documentation: /pkg/README.md\n"));
        assert!(prompt.contains("- Examples: /pkg/examples (extensions, custom tools, SDK)\n"));
    }

    #[test]
    fn empty_custom_prompt_falls_back_to_default() {
        let options = BuildSystemPromptOptions {
            custom_prompt: Some(String::new()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        assert!(build_system_prompt(&options).starts_with("You are an expert coding assistant"));
    }

    // ---- #9548: section patches (system-prompt-updates.test.ts) ----------

    fn section_options(sections: &[(&str, &str)], cwd: &str) -> BuildSystemPromptOptions {
        let mut map = serde_json::Map::new();
        for (name, content) in sections {
            map.insert(
                (*name).to_owned(),
                serde_json::Value::String((*content).to_owned()),
            );
        }
        BuildSystemPromptOptions {
            sections: Some(map),
            cwd: PathBuf::from(cwd),
            ..Default::default()
        }
    }

    /// `diffs sections into a patch` (system-prompt-updates.test.ts:83-93):
    /// only changed sections ride the patch; a dropped section becomes a
    /// `null` removal marker; identical inputs produce no patch.
    /// #9548 `toolGuidelines`: per-tool guideline lines follow the CURRENT
    /// selected tool set (system-prompt.ts:111) — a mid-run loadout
    /// change re-derives the rules section without rebuilding the map.
    #[test]
    fn tool_guidelines_follow_the_selected_tool_set() {
        let mut tool_guidelines: HashMap<String, Vec<String>> = HashMap::new();
        tool_guidelines.insert("read".to_owned(), vec!["read first".to_owned()]);
        tool_guidelines.insert("grep".to_owned(), vec!["grep early".to_owned()]);
        let options = BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".to_owned()]),
            tool_guidelines: Some(tool_guidelines.clone()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        let prompt = build_system_prompt(&options);
        assert!(prompt.contains("- read first"));
        assert!(!prompt.contains("- grep early"));

        // Same map, new selection: the guideline set re-derives.
        let options = BuildSystemPromptOptions {
            selected_tools: Some(vec!["read".to_owned(), "grep".to_owned()]),
            tool_guidelines: Some(tool_guidelines),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        let prompt = build_system_prompt(&options);
        let read_at = prompt.find("- read first").expect("read rule");
        let grep_at = prompt.find("- grep early").expect("grep rule");
        assert!(
            read_at < grep_at,
            "per-tool lines render in selected-tool order"
        );
    }

    #[test]
    fn diff_system_prompt_sections_patches_by_name() {
        let previous =
            build_system_prompt_sections(&section_options(&[("plan_mode", "Plan only.")], "/tmp"));
        let current = build_system_prompt_sections(&section_options(
            &[("plan_mode", "Implementation allowed.")],
            "/tmp",
        ));
        let patch =
            diff_system_prompt_sections(&previous, &current).expect("patch for a changed section");
        assert_eq!(
            patch.get("plan_mode").and_then(serde_json::Value::as_str),
            Some("<plan_mode>\nImplementation allowed.\n</plan_mode>")
        );
        assert_eq!(patch.len(), 1);
        assert_eq!(diff_system_prompt_sections(&previous, &previous), None);
        let without = build_system_prompt_sections(&section_options(&[], "/tmp"));
        let removal =
            diff_system_prompt_sections(&previous, &without).expect("patch for a removed section");
        assert_eq!(
            removal.get("plan_mode").map(serde_json::Value::is_null),
            Some(true)
        );
    }

    /// `keeps the preamble untagged and replaces it like any section`
    /// (system-prompt-updates.test.ts:95-108): `preamble` is raw text and
    /// diffs by name like any other section; a forced prompt is opaque
    /// `content` with no sections.
    #[test]
    fn preamble_is_untagged_and_forced_prompt_is_opaque() {
        let previous = build_system_prompt_sections(&BuildSystemPromptOptions {
            custom_prompt: Some("You are A.".to_owned()),
            cwd: PathBuf::from("/tmp"),
            ..Default::default()
        });
        let current = build_system_prompt_sections(&BuildSystemPromptOptions {
            custom_prompt: Some("You are B.".to_owned()),
            cwd: PathBuf::from("/tmp"),
            ..Default::default()
        });
        assert_eq!(
            previous
                .iter()
                .find(|(name, _)| name == "preamble")
                .map(|(_, text)| text.as_str()),
            Some("You are A.")
        );
        let patch = diff_system_prompt_sections(&previous, &current).expect("patch");
        assert_eq!(
            patch.get("preamble").and_then(serde_json::Value::as_str),
            Some("You are B.")
        );
        assert_eq!(patch.len(), 1);

        let forced = build_system_prompt_state(&BuildSystemPromptOptions {
            force_system_prompt: Some("Exact prompt.".to_owned()),
            cwd: PathBuf::from("/tmp"),
            ..Default::default()
        });
        assert_eq!(forced.content, "Exact prompt.");
        assert!(forced.sections.is_none());
        let unforced = build_system_prompt_state(&BuildSystemPromptOptions {
            cwd: PathBuf::from("/tmp"),
            ..Default::default()
        });
        assert_eq!(unforced.content, "");
        assert_eq!(
            unforced.sections.as_ref().map(|sections| sections.len()),
            Some(
                build_system_prompt_sections(&BuildSystemPromptOptions {
                    cwd: PathBuf::from("/tmp"),
                    ..Default::default()
                })
                .len()
            )
        );
    }

    /// Custom sections replace built-ins by name and `preamble` is rejected
    /// (system-prompt.ts:156-160 + rpi boundary-warn deviation).
    #[test]
    fn custom_sections_replace_by_name_and_preamble_is_invalid() {
        assert!(!is_valid_section_name("preamble"));
        assert!(!is_valid_section_name("1bad"));
        assert!(!is_valid_section_name("Bad"));
        assert!(is_valid_section_name("plan_mode"));
        let sections =
            build_system_prompt_sections(&section_options(&[("rules", "- custom rules")], "/tmp"));
        // Custom content replaces the built-in body; the tag wrapping is
        // applied uniformly to every non-preamble section.
        assert_eq!(
            sections
                .iter()
                .find(|(name, _)| name == "rules")
                .map(|(_, text)| text.as_str()),
            Some("<rules>\n- custom rules\n</rules>")
        );
        // A valid custom section survives alongside the built-ins.
        let sections =
            build_system_prompt_sections(&section_options(&[("plan_mode", "Plan only.")], "/tmp"));
        assert!(sections
            .iter()
            .any(|(name, text)| name == "plan_mode"
                && text == "<plan_mode>\nPlan only.\n</plan_mode>"));
    }

    #[test]
    fn cwd_backslashes_normalised() {
        let options = BuildSystemPromptOptions {
            custom_prompt: Some("C".to_string()),
            cwd: PathBuf::from("C:\\Users\\dev"),
            ..Default::default()
        };
        // #9548: both branches end with the tagged `<cwd>` section.
        assert!(build_system_prompt(&options).ends_with("<cwd>\nC:/Users/dev\n</cwd>"));
        let options = BuildSystemPromptOptions {
            custom_prompt: None,
            cwd: PathBuf::from("C:\\Users\\dev"),
            ..Default::default()
        };
        assert!(build_system_prompt(&options).ends_with(
            "<cwd>
C:/Users/dev
</cwd>"
        ));
    }

    // ---- resolve_prompt_input ----------------------------------------------

    #[test]
    fn resolve_prompt_input_falsy() {
        assert_eq!(resolve_prompt_input(None, "system prompt"), None);
        assert_eq!(resolve_prompt_input(Some(""), "system prompt"), None);
    }

    #[test]
    fn resolve_prompt_input_inline_for_missing_path() {
        assert_eq!(
            resolve_prompt_input(Some("definitely/not/a/real/path.md"), "system prompt"),
            Some("definitely/not/a/real/path.md".to_string())
        );
    }
}
