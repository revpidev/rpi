//! Port of `packages/coding-agent/src/core/system-prompt.ts`
//! @ pi 0.82.1 (2efa728), plus the context-file and system-prompt-source
//! parts of `packages/coding-agent/src/core/resource-loader.ts`:
//! `resolvePromptInput` (:50-65), `loadContextFileFromDir` (:67-86),
//! `loadProjectContextFiles` (:88-123), `discoverSystemPromptFile`
//! (:969-981) and `discoverAppendSystemPromptFile` (:983-995).
//!
//! Context files: per directory the first hit of `AGENTS.md`, `AGENTS.MD`,
//! `CLAUDE.md`, `CLAUDE.MD` (in that priority order) wins. Loading order is
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
//!   anchored at pi's package dir). pir has no bundled package docs yet, so
//!   the three paths arrive via [`BuildSystemPromptOptions::doc_paths`];
//!   `None` omits the whole "Pi documentation" paragraph. The surrounding
//!   assembly (tools list, guidelines, context injection, cwd line) is
//!   byte-faithful.
//! - `cwd`/`agent_dir` inputs are resolved with `resolve_path` inside each
//!   filesystem-touching function; upstream resolves them once in the
//!   `DefaultResourceLoader` constructor (resource-loader.ts:218-219).
//! - Read failures are logged with `tracing::warn!` instead of
//!   `console.error(chalk.yellow(...))`.
//! - `.pir` rename per ADR-0001 (`CONFIG_DIR_NAME`, `SYSTEM.md` /
//!   `APPEND_SYSTEM.md` live under it project-side).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config;
use crate::tools::path_utils::resolve_path;

/// A loaded context file (`{ path, content }`, resource-loader.ts:67).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    /// `join(dir, filename)` — the directory the file was found in, joined
    /// with the winning candidate name.
    pub path: PathBuf,
    /// UTF-8 file content.
    pub content: String,
}

/// Candidate file names, in priority order (resource-loader.ts:68).
const CONTEXT_FILE_CANDIDATES: [&str; 4] = ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------
// Context files (resource-loader.ts:67-123)
// ---------------------------------------------------------------------------

/// `loadContextFileFromDir` (resource-loader.ts:67-86): return the first
/// readable candidate of `AGENTS.md` > `AGENTS.MD` > `CLAUDE.md` >
/// `CLAUDE.MD` in `dir`. A candidate that exists but is not a file is
/// skipped silently; a stat/read failure logs a warning and falls through
/// to the next candidate (upstream `try/catch` around `statSync` +
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

/// `loadProjectContextFiles` (resource-loader.ts:88-123).
///
/// Order: the global agent-dir context file first, then the ancestor chain
/// from the filesystem root down to `cwd` (root side first, cwd last —
/// upstream `unshift`). Paths are deduplicated; loading happens regardless
/// of project trust.
pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let resolved_cwd = resolve_path(&cwd.to_string_lossy(), &process_cwd());
    let resolved_agent_dir = resolve_path(&agent_dir.to_string_lossy(), &process_cwd());

    let mut context_files: Vec<ContextFile> = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    if let Some(global_context) = load_context_file_from_dir(&resolved_agent_dir) {
        seen_paths.insert(global_context.path.clone());
        context_files.push(global_context);
    }

    let mut ancestor_context_files: Vec<ContextFile> = Vec::new();
    let mut current_dir = resolved_cwd;

    loop {
        if let Some(context_file) = load_context_file_from_dir(&current_dir) {
            if !seen_paths.contains(&context_file.path) {
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
/// `SYSTEM.md` (`{cwd}/.pir/SYSTEM.md`) wins but requires project trust;
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
// System prompt assembly (system-prompt.ts:8-162)
// ---------------------------------------------------------------------------

/// Bundled documentation paths for the default system prompt's "Pi
/// documentation" paragraph (upstream: `getReadmePath` / `getDocsPath` /
/// `getExamplesPath`, config.ts:427-439). Supplied by the caller because
/// pir has no bundled package docs dir yet (see module header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPaths {
    /// Main documentation (README.md).
    pub readme_path: String,
    /// Additional docs directory.
    pub docs_path: String,
    /// Examples directory.
    pub examples_path: String,
}

/// `BuildSystemPromptOptions` (system-prompt.ts:8-25), with `skills`
/// replaced by the pre-formatted [`Self::skills_xml`] slot and the bundled
/// doc paths made explicit via [`Self::doc_paths`] (see module header).
#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptOptions {
    /// Custom system prompt (replaces the default).
    pub custom_prompt: Option<String>,
    /// Tools to include in the prompt. Default: `["read", "bash", "edit",
    /// "write"]` (system-prompt.ts:81).
    pub selected_tools: Option<Vec<String>>,
    /// Optional one-line tool snippets keyed by tool name.
    pub tool_snippets: Option<HashMap<String, String>>,
    /// Additional guideline bullets appended to the default guidelines.
    pub prompt_guidelines: Vec<String>,
    /// Text to append to the system prompt.
    pub append_system_prompt: Option<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Pre-loaded context files.
    pub context_files: Vec<ContextFile>,
    /// Pre-formatted skills XML section (output of the skills module's
    /// `format_skills_for_prompt`, not yet ported). A non-empty value plays
    /// the role of upstream `skills.length > 0`.
    pub skills_xml: Option<String>,
    /// Bundled documentation paths for the default prompt; `None` omits
    /// the "Pi documentation" paragraph (pir difference, see module
    /// header).
    pub doc_paths: Option<DocPaths>,
}

/// The `<project_context>` injection block (system-prompt.ts:54-61 and
/// :145-152 — both branches share this exact format).
fn append_project_context(prompt: &mut String, context_files: &[ContextFile]) {
    if context_files.is_empty() {
        return;
    }
    prompt.push_str("\n\n<project_context>\n\n");
    prompt.push_str("Project-specific instructions and guidelines:\n\n");
    for file in context_files {
        prompt.push_str("<project_instructions path=\"");
        prompt.push_str(&file.path.display().to_string());
        prompt.push_str("\">\n");
        prompt.push_str(&file.content);
        prompt.push_str("\n</project_instructions>\n\n");
    }
    prompt.push_str("</project_context>\n");
}

/// `buildSystemPrompt` (system-prompt.ts:28-162): build the system prompt
/// with tools, guidelines, and context. The skills section is appended only
/// when the `read` tool is available and [`BuildSystemPromptOptions::skills_xml`]
/// is non-empty.
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    // cwd.replace(/\\/g, "/")
    let prompt_cwd = options.cwd.to_string_lossy().replace('\\', "/");

    // `appendSystemPrompt ? \n\n${appendSystemPrompt} : ""` — empty string
    // is falsy upstream.
    let append_section = options
        .append_system_prompt
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\n{s}"))
        .unwrap_or_default();

    let skills_xml = options.skills_xml.as_deref().filter(|s| !s.is_empty());

    // `if (customPrompt)` — an empty custom prompt falls through to the
    // default branch upstream (empty string is falsy).
    if let Some(custom_prompt) = options.custom_prompt.as_deref().filter(|s| !s.is_empty()) {
        let mut prompt = custom_prompt.to_string();
        prompt.push_str(&append_section);

        append_project_context(&mut prompt, &options.context_files);

        // Skills section only if the read tool is available.
        let custom_prompt_has_read = match &options.selected_tools {
            None => true,
            Some(tools) => tools.iter().any(|name| name == "read"),
        };
        if custom_prompt_has_read {
            if let Some(skills_xml) = skills_xml {
                prompt.push_str(skills_xml);
            }
        }

        prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));
        return prompt;
    }

    // Build the tools list based on the selected tools. A tool appears in
    // Available tools only when the caller provides a (truthy, i.e.
    // non-empty) one-line snippet.
    const DEFAULT_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];
    let tools: Vec<&str> = match &options.selected_tools {
        Some(selected) => selected.iter().map(String::as_str).collect(),
        None => DEFAULT_TOOLS.to_vec(),
    };
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
        "(none)".to_string()
    } else {
        visible_tools
            .iter()
            .map(|name| format!("- {name}: {}", snippet(name).unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Build guidelines based on which tools are actually available.
    let mut guidelines_list: Vec<String> = Vec::new();
    let mut guidelines_set: HashSet<String> = HashSet::new();
    let mut add_guideline = |guideline: &str| {
        if guidelines_set.insert(guideline.to_string()) {
            guidelines_list.push(guideline.to_string());
        }
    };

    let has_bash = tools.contains(&"bash");
    let has_grep = tools.contains(&"grep");
    let has_find = tools.contains(&"find");
    let has_ls = tools.contains(&"ls");
    let has_read = tools.contains(&"read");

    // File exploration guidelines.
    if has_bash && !has_grep && !has_find && !has_ls {
        add_guideline("Use bash for file operations like ls, rg, find");
    }

    for guideline in &options.prompt_guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() {
            add_guideline(normalized);
        }
    }

    // Always include these.
    add_guideline("Be concise in your responses");
    add_guideline("Show file paths clearly when working with files");

    let guidelines = guidelines_list
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n{guidelines}"
    );

    if let Some(doc_paths) = &options.doc_paths {
        prompt.push_str(&format!(
            "\n\nPi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: {}\n- Additional docs: {}\n- Examples: {} (extensions, custom tools, SDK)\n- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md), environment variables (docs/environment-variables.md)\n- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
            doc_paths.readme_path, doc_paths.docs_path, doc_paths.examples_path
        ));
    }

    prompt.push_str(&append_section);

    append_project_context(&mut prompt, &options.context_files);

    // Skills section only if the read tool is available.
    if has_read {
        if let Some(skills_xml) = skills_xml {
            prompt.push_str(skills_xml);
        }
    }

    prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));

    prompt
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
        assert_eq!(
            prompt,
            "CUSTOM\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/agent/AGENTS.md\">\nglobal rules\n</project_instructions>\n\n<project_instructions path=\"/repo/AGENTS.md\">\nproject rules\n</project_instructions>\n\n</project_context>\n\nCurrent working directory: /repo"
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
            "CUSTOM\nCurrent working directory: /repo"
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
        assert!(prompt.starts_with("CUSTOM\n\nEXTRA\n\n<project_context>"));
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
        // Only snippet-bearing tools are listed (edit/write have none).
        assert!(prompt.contains("Available tools:\n- read: Read a file\n- bash: Run a command\n"));
        // Default tools include bash but not grep/find/ls → exploration guideline.
        assert!(prompt.contains("- Use bash for file operations like ls, rg, find\n"));
        assert!(prompt.contains(
            "- Be concise in your responses\n- Show file paths clearly when working with files"
        ));
        assert!(prompt.ends_with("\nCurrent working directory: /repo"));
    }

    #[test]
    fn default_prompt_no_visible_tools() {
        let options = BuildSystemPromptOptions {
            tool_snippets: Some(HashMap::new()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        assert!(build_system_prompt(&options).contains("Available tools:\n(none)\n"));
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
        let guidelines = prompt
            .split("Guidelines:\n")
            .nth(1)
            .expect("guidelines section");
        assert_eq!(guidelines.matches("- Extra rule").count(), 1);
        assert_eq!(
            guidelines.matches("Be concise in your responses").count(),
            1
        );
    }

    #[test]
    fn skills_gate_requires_read_tool() {
        let base = BuildSystemPromptOptions {
            custom_prompt: Some("CUSTOM".to_string()),
            skills_xml: Some("<skills>XML</skills>".to_string()),
            cwd: PathBuf::from("/repo"),
            ..Default::default()
        };
        assert!(build_system_prompt(&base).contains("<skills>XML</skills>"));
        // read not selected → skills dropped (custom branch).
        let options = BuildSystemPromptOptions {
            selected_tools: Some(vec!["bash".to_string()]),
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
        assert!(!build_system_prompt(&options).contains("Pi documentation"));
        let options = BuildSystemPromptOptions {
            doc_paths: Some(DocPaths {
                readme_path: "/pkg/README.md".to_string(),
                docs_path: "/pkg/docs".to_string(),
                examples_path: "/pkg/examples".to_string(),
            }),
            ..options
        };
        let prompt = build_system_prompt(&options);
        assert!(prompt.contains("Pi documentation (read only when the user asks about pi itself"));
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

    #[test]
    fn cwd_backslashes_normalised() {
        let options = BuildSystemPromptOptions {
            custom_prompt: Some("C".to_string()),
            cwd: PathBuf::from("C:\\Users\\dev"),
            ..Default::default()
        };
        assert!(build_system_prompt(&options).ends_with("Current working directory: C:/Users/dev"));
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
