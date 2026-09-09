//! Agent discovery: builtin < user < project, frontmatter → AgentConfig,
//! settings overrides, alias resolution.
//!
//! Port of pi-subagents `src/agents/agents.ts` + `src/agents/agent-selection.ts`
//! + `src/agents/identity.ts` @ v0.48.0 (56f97234), P0 subset:
//! - discovery order: builtin → user (`RPI_SUBAGENT_EXTRA_AGENT_DIRS`,
//!   `<agentDir>/agents`, `~/.agents`) → project (`<root>/.agents` legacy +
//!   `<root>/.rpi/agents` preferred); the "installed package" level is P2
//!   ([DEFER], requirements §2.1) and contributes nothing here.
//! - same-name override: project > user > builtin (`mergeAgentsForScope`).
//! - settings overrides: builtin full replace (project > user, bulk
//!   disableBuiltins), custom agents fill-only (frontmatter wins).
//!
//! Intentional differences: `.pi`/`~/.pi` → `.rpi`/`~/.rpi` (ADR-0001);
//! `PI_SUBAGENT_EXTRA_AGENT_DIRS` → `RPI_SUBAGENT_EXTRA_AGENT_DIRS`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::paths;

/// Frontmatter `thinking`: unset | explicitly disabled (`false`) | a level.
#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingSpec {
    Unset,
    Disabled,
    Level(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentSource {
    Builtin,
    User,
    Project,
}

impl AgentSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentSource::Builtin => "builtin",
            AgentSource::User => "user",
            AgentSource::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMode {
    Fresh,
    Fork,
}

impl ContextMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextMode::Fresh => "fresh",
            ContextMode::Fork => "fork",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Runtime name: `{package}.{local}` when a package is set (identity.ts:19-22).
    pub name: String,
    pub local_name: String,
    pub package_name: Option<String>,
    pub description: String,
    pub aliases: Option<Vec<String>>,
    /// `None` = frontmatter did not declare `tools` (child inherits host
    /// defaults); `Some(list)` = explicit allowlist (empty → `--no-tools`).
    pub tools: Option<Vec<String>>,
    pub mcp_direct_tools: Vec<String>,
    pub model: Option<String>,
    pub fallback_models: Vec<String>,
    pub thinking: ThinkingSpec,
    pub system_prompt_mode: &'static str,
    pub inherit_project_context: bool,
    pub inherit_skills: bool,
    pub default_context: Option<ContextMode>,
    pub default_async: Option<bool>,
    pub default_timeout_ms: Option<u64>,
    pub system_prompt: String,
    pub source: AgentSource,
    pub file_path: PathBuf,
    pub skills: Vec<String>,
    pub extensions: Option<Vec<String>>,
    pub subagent_only_extensions: Option<Vec<String>>,
    pub output: Option<String>,
    pub default_reads: Vec<String>,
    pub default_progress: bool,
    pub max_subagent_depth: Option<u64>,
    pub disabled: Option<bool>,
    /// `acceptanceRole`: "read-only" | "writer" (FR-P1-09 acceptance level
    /// inference input; agent-management.ts:559-562).
    pub acceptance_role: Option<String>,
    /// `memory: {scope: project|user, path}` (agent-memory.ts:36): the
    /// per-agent memory dir whose MEMORY.md is injected into the child prompt.
    pub memory: Option<MemoryConfig>,
    /// Which frontmatter keys the definition actually wrote (agentFrontmatterFields
    /// WeakMap upstream) — the fill-only override guard.
    pub frontmatter_fields: std::collections::BTreeSet<String>,
}

impl AgentConfig {
    pub fn source_str(&self) -> &'static str {
        self.source.as_str()
    }

    fn has_frontmatter_field(&self, fields: &[&str]) -> bool {
        fields.iter().any(|f| self.frontmatter_fields.contains(*f))
    }
}

/// `memory` frontmatter (agent-memory.ts:36): inline
/// `memory: {scope: "project"|"user", path: "..."}` — the hand-rolled
/// frontmatter yields scalar strings, so the object form arrives as the raw
/// line; both `memory: {scope: project, path: notes}` and
/// `memory: project:notes` (shorthand) are accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryConfig {
    /// `project` → `<cwd>/.rpi/agent-memory/`, `user` → `<agentDir>/agent-memory/`.
    pub scope: &'static str,
    pub path: String,
}

impl MemoryConfig {
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        let raw = raw?.trim();
        if raw.is_empty() || raw == "false" {
            return None;
        }
        let body = raw.trim_start_matches('{').trim_end_matches('}').trim();
        // Try `key: value` pairs first.
        let mut scope: Option<&'static str> = None;
        let mut path: Option<String> = None;
        for pair in body.split(',') {
            let Some((key, value)) = pair.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key {
                "scope" => {
                    scope = match value {
                        "project" => Some("project"),
                        "user" => Some("user"),
                        _ => Some("project"),
                    };
                }
                "path" => path = Some(value.to_string()),
                _ => {}
            }
        }
        // Shorthand `project:notes`.
        if scope.is_none() || path.is_none() {
            if let Some((raw_scope, raw_path)) = body.split_once(':') {
                let raw_scope = raw_scope.trim();
                let raw_path = raw_path.trim().trim_matches('"');
                if scope.is_none() {
                    scope = match raw_scope {
                        "user" => Some("user"),
                        _ => Some("project"),
                    };
                }
                if path.is_none() && !raw_path.is_empty() {
                    path = Some(raw_path.to_string());
                }
            }
        }
        Some(Self {
            scope: scope.unwrap_or("project"),
            path: path.unwrap_or_else(|| "default".to_string()),
        })
    }

    /// `resolveMemoryDir` (agent-memory.ts:79-103): traversal-guarded
    /// directory under the scope root.
    pub fn resolve_dir(&self, cwd: &Path, agent_name: &str) -> Option<PathBuf> {
        if self.path.is_empty()
            || self.path.contains('\0')
            || self
                .path
                .split(['/'])
                .any(|segment| segment == "." || segment == "..")
            || self.path.contains(':')
        {
            return None;
        }
        let base = if self.scope == "user" {
            crate::paths::get_agent_dir().join("agent-memory")
        } else {
            crate::paths::get_project_config_dir(cwd).join("agent-memory")
        };
        Some(
            base.join(&self.path)
                .join(sanitize_memory_segment(agent_name)),
        )
    }
}

fn sanitize_memory_segment(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `readMemoryFile` (agent-memory.ts:143-171): first MAX_MEMORY_LINES (200)
/// lines, MAX_MEMORY_BYTES (16KiB) cap; O_NOFOLLOW-equivalent (symlinked
/// memory files are skipped — "unsafe").
pub fn read_agent_memory_file(dir: &Path) -> Option<String> {
    let file = dir.join("MEMORY.md");
    let meta = std::fs::symlink_metadata(&file).ok()?;
    if meta.file_type().is_symlink() {
        return None; // unsafe: symlinked memory file
    }
    let content = std::fs::read_to_string(&file).ok()?;
    let mut out = String::new();
    for (index, line) in content.lines().enumerate() {
        if index >= 200 || out.len() + line.len() > 16 * 1024 {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `buildAgentMemoryInjection` (agent-memory.ts:193-224): read-write block
/// when the agent holds write tools, read-only recall otherwise.
pub fn build_agent_memory_injection(memory_text: &str, writable: bool) -> String {
    if writable {
        format!(
            "<agent_memory access=\"read-write\">\nMaintain durable notes in your agent memory.\nCreate and update MEMORY.md in your memory directory as you learn durable facts, decisions, and conventions.\n\n{memory_text}\n</agent_memory>"
        )
    } else {
        format!(
            "<agent_memory access=\"read-only\">\nRecall durable notes from a prior run of this agent.\n\n{memory_text}\n</agent_memory>"
        )
    }
}

pub const EXTRA_AGENT_DIRS_ENV: &str = "RPI_SUBAGENT_EXTRA_AGENT_DIRS";

fn default_system_prompt_mode(local_name: &str) -> &'static str {
    // agents.ts:48-50: delegate defaults to append, everything else to replace.
    if local_name == "delegate" {
        "append"
    } else {
        "replace"
    }
}

fn default_inherit_project_context(local_name: &str) -> bool {
    // agents.ts:52-54.
    local_name == "delegate"
}

/// `parsePackageName` + `normalizePackageName` (identity.ts:5-16).
fn parse_package_name(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "false" {
        return Ok(None);
    }
    let lowered = trimmed.to_lowercase();
    let mut collapsed = String::new();
    let mut last_was_ws = false;
    for c in lowered.chars() {
        if c.is_whitespace() {
            if !last_was_ws {
                collapsed.push('-');
            }
            last_was_ws = true;
        } else {
            last_was_ws = false;
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' {
                collapsed.push(c);
            }
        }
    }
    // Collapse `-+` / `.+` runs and trim edge separators.
    let mut normalized = String::new();
    let mut prev: Option<char> = None;
    for c in collapsed.chars() {
        match (prev, c) {
            (Some('-'), '-') | (Some('.'), '.') => {}
            _ => normalized.push(c),
        }
        prev = Some(c);
    }
    let trimmed = normalized.trim_matches(['-', '.']);
    if trimmed.is_empty() || !valid_identifier(trimmed) {
        return Err("is invalid after sanitization.".to_string());
    }
    Ok(Some(trimmed.to_string()))
}

/// `IDENTIFIER_PATTERN` (identity.ts:3): `^[a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)*$`.
fn valid_identifier(value: &str) -> bool {
    fn segment(s: &str) -> bool {
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
            _ => return false,
        }
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }
    let mut parts = value.split('.');
    let mut all = false;
    for part in parts.by_ref() {
        if !segment(part) {
            return false;
        }
        all = true;
    }
    all
}

/// `buildRuntimeName` (identity.ts:19-22).
fn build_runtime_name(local_name: &str, package_name: Option<&str>) -> String {
    match package_name.map(str::trim).filter(|p| !p.is_empty()) {
        Some(package) => format!("{package}.{local_name}"),
        None => local_name.to_string(),
    }
}

/// `normalizeAgentAliases` (agents.ts:495-499).
fn normalize_aliases(raw: Option<Vec<String>>, runtime_name: &str) -> Option<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for alias in raw.unwrap_or_default() {
        let alias = alias.trim();
        if alias.is_empty() || alias == runtime_name || !seen.insert(alias.to_string()) {
            continue;
        }
        out.push(alias.to_string());
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `splitToolList` (agents.ts:531-545).
fn split_tool_list(raw: Option<Vec<String>>) -> (Option<Vec<String>>, Vec<String>) {
    let mut tools = Vec::new();
    let mut mcp = Vec::new();
    for tool in raw.clone().unwrap_or_default() {
        if let Some(name) = tool.strip_prefix("mcp:") {
            mcp.push(name.to_string());
        } else {
            tools.push(tool);
        }
    }
    (raw.map(|_| tools), mcp)
}

/// One skipped agent definition or directory entry (additive discovery
/// diagnostic, R7.1.3.1; upstream `AgentDiscoveryDiagnostic`,
/// agents.ts:245-250). `path` is the file/directory that failed, `scope` is
/// the discovery source it was found under and `error` is the parse/IO error
/// summary (never file contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverDiagnostic {
    pub path: PathBuf,
    pub scope: AgentSource,
    pub error: String,
}

/// Frontmatter → AgentConfig (`loadAgentsFromDir` body, agents.ts:1510-1656).
/// `Err` mirrors the upstream throws (invalid async/timeoutMs/package); the
/// caller records it as a [`DiscoverDiagnostic`] and continues.
/// `Ok(None)` = file skipped (missing name/description) exactly like the
/// upstream `continue`.
pub fn agent_from_content(
    content: &str,
    file_path: &Path,
    source: AgentSource,
) -> Result<Option<AgentConfig>, String> {
    let parsed = super::frontmatter::parse_frontmatter(content);
    let fm = &parsed.frontmatter;

    let Some(local_name) = fm.get("name").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let Some(description) = fm.get("description").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    let package_name = match parse_package_name(fm.get("package").map(String::as_str)) {
        Ok(package) => package,
        // Upstream `parsePackageName(frontmatter.package, `Agent 'x' package`)`
        // throws since #1200 (e973fa3c); the caller turns it into a diagnostic.
        Err(error) => return Err(format!("Agent '{local_name}' package {error}")),
    };
    let runtime_name = build_runtime_name(local_name, package_name.as_deref());

    let default_async = match fm.get("async").map(String::as_str) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => {
            return Err(format!(
                "Agent '{local_name}' has invalid async frontmatter; expected true or false."
            ))
        }
        None => None,
    };
    let default_timeout_ms = match fm.get("timeoutMs").map(String::as_str) {
        Some(value) => match value.parse::<i64>() {
            Ok(parsed) if parsed > 0 => Some(parsed as u64),
            _ => {
                return Err(format!(
                    "Agent '{local_name}' has invalid timeoutMs frontmatter; expected a positive integer."
                ))
            }
        },
        None => None,
    };

    let raw_tools = super::frontmatter::parse_frontmatter_list(fm.get("tools").map(String::as_str));
    let (tools, mcp_direct_tools) = split_tool_list(raw_tools);
    let default_reads =
        super::frontmatter::parse_frontmatter_list(fm.get("defaultReads").map(String::as_str))
            .unwrap_or_default();
    let raw_aliases = super::frontmatter::parse_frontmatter_list(
        fm.get("aliases")
            .or_else(|| fm.get("alias"))
            .map(String::as_str),
    );
    let aliases = normalize_aliases(raw_aliases, &runtime_name);
    let skills = super::frontmatter::parse_frontmatter_list(
        fm.get("skill")
            .or_else(|| fm.get("skills"))
            .map(String::as_str),
    )
    .unwrap_or_default();
    let fallback_models =
        super::frontmatter::parse_frontmatter_list(fm.get("fallbackModels").map(String::as_str))
            .unwrap_or_default();

    let system_prompt_mode = match fm.get("systemPromptMode").map(String::as_str) {
        Some("replace") => "replace",
        Some("append") => "append",
        _ => default_system_prompt_mode(local_name),
    };
    let inherit_project_context = match fm.get("inheritProjectContext").map(String::as_str) {
        Some("true") => true,
        Some("false") => false,
        _ => default_inherit_project_context(local_name),
    };
    let inherit_skills = match fm.get("inheritSkills").map(String::as_str) {
        Some("true") => true,
        Some("false") => false,
        _ => false,
    };
    let default_context = match fm.get("defaultContext").map(String::as_str) {
        Some("fork") => Some(ContextMode::Fork),
        Some("fresh") => Some(ContextMode::Fresh),
        _ => None,
    };
    let thinking = match fm.get("thinking").map(String::as_str) {
        Some("false") => ThinkingSpec::Disabled,
        Some(level) => ThinkingSpec::Level(level.to_string()),
        None => ThinkingSpec::Unset,
    };
    let max_subagent_depth = match fm.get("maxSubagentDepth").map(String::as_str) {
        // `Number.isInteger(parsed) && parsed >= 0` — invalid values are
        // ignored (undefined), not fatal (agents.ts:1592, 1614-1616).
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .map(|v| v as u64),
        None => None,
    };
    let extensions =
        super::frontmatter::parse_frontmatter_list(fm.get("extensions").map(String::as_str));
    let subagent_only_extensions = super::frontmatter::parse_frontmatter_list(
        fm.get("subagentOnlyExtensions").map(String::as_str),
    );

    Ok(Some(AgentConfig {
        name: runtime_name,
        local_name: local_name.to_string(),
        package_name,
        description: description.to_string(),
        aliases,
        tools,
        mcp_direct_tools,
        model: fm.get("model").cloned(),
        fallback_models,
        thinking,
        system_prompt_mode,
        inherit_project_context,
        inherit_skills,
        default_context,
        default_async,
        default_timeout_ms,
        system_prompt: parsed.body,
        source,
        file_path: file_path.to_path_buf(),
        skills,
        extensions,
        subagent_only_extensions,
        output: fm.get("output").cloned(),
        default_reads,
        default_progress: fm.get("defaultProgress").map(String::as_str) == Some("true"),
        max_subagent_depth,
        disabled: None,
        memory: MemoryConfig::parse(fm.get("memory").map(String::as_str)),
        acceptance_role: match fm.get("acceptanceRole").map(String::as_str) {
            // read-only | writer | false; anything else is treated as unset
            // (agent-management.ts:559-562 validation vocabulary).
            Some("read-only") | Some("writer") => fm.get("acceptanceRole").cloned(),
            _ => None,
        },
        frontmatter_fields: fm.keys().cloned().collect(),
    }))
}

/// Load agents from one directory (`loadAgentsFromDir`, agents.ts:2166-2168 +
/// `loadAgentsFromDefinitionFiles`, agents.ts:1960-2163). Per-file failures
/// are isolated: they become [`DiscoverDiagnostic`] entries and the remaining
/// files still load (#1200, e973fa3c). Diagnostics keep the traversal order
/// (name-sorted, depth-first) so callers can assert them stably.
pub fn load_agents_from_dir_with_diagnostics(
    dir: &Path,
    source: &str,
) -> (Vec<AgentConfig>, Vec<DiscoverDiagnostic>) {
    let source = match source {
        "builtin" => AgentSource::Builtin,
        "user" => AgentSource::User,
        "project" => AgentSource::Project,
        _ => AgentSource::User,
    };
    let mut agents = Vec::new();
    let mut diagnostics = Vec::new();
    for file_path in collect_files_recursive(dir, root_predicate(), source, &mut diagnostics) {
        if is_legacy_agent_skill_path(dir, &file_path) {
            continue;
        }
        let content = match std::fs::read_to_string(&file_path) {
            Ok(content) => content,
            Err(error) => {
                diagnostics.push(DiscoverDiagnostic {
                    path: file_path,
                    scope: source,
                    error: format!("cannot read agent definition: {error}"),
                });
                continue;
            }
        };
        match agent_from_content(&content, &file_path, source) {
            Ok(Some(agent)) => agents.push(agent),
            // Missing name/description: upstream `continue`, no diagnostic.
            Ok(None) => {}
            Err(error) => diagnostics.push(DiscoverDiagnostic {
                path: file_path,
                scope: source,
                error,
            }),
        }
    }
    (agents, diagnostics)
}

/// Backward-compatible wrapper over
/// [`load_agents_from_dir_with_diagnostics`]: same signature and return shape
/// as before (A-R2), diagnostics ignored.
#[allow(dead_code)] // backward-compatible API surface (A-R2); exercised in tests
pub fn load_agents_from_dir(dir: &Path, source: &str) -> Result<Vec<AgentConfig>, String> {
    Ok(load_agents_from_dir_with_diagnostics(dir, source).0)
}

fn root_predicate() -> fn(&str) -> bool {
    // `.md` but not `.chain.md` (agents.ts:1500).
    |file_name: &str| file_name.ends_with(".md") && !file_name.ends_with(".chain.md")
}

// `DISCOVERY_PRUNED_DIR_NAMES` (agents.ts:1678, #1596/671bc27c): `.pi` maps
// to `.rpi` per ADR-0001; `sync-backups` is the operational backup dir.
const DISCOVERY_PRUNED_DIR_NAMES: [&str; 4] = [".git", "node_modules", ".rpi", "sync-backups"];

fn should_prune_discovery_dir(root_dir: &Path, dir: &Path, dir_name: &str) -> bool {
    // agents.ts:1684-1688.
    if DISCOVERY_PRUNED_DIR_NAMES.contains(&dir_name) {
        return true;
    }
    if dir.join(".git").exists() {
        return true;
    }
    dir != root_dir && is_project_root_candidate(dir)
}

fn is_project_root_candidate(dir: &Path) -> bool {
    // agents.ts:626.
    paths::get_project_config_dir(dir).is_dir() || dir.join(".agents").is_dir()
}

/// `listFilesRecursive` (agents.ts:1690-1731): name-sorted (byte order ≈
/// localeCompare for ASCII), depth-first, pruned dirs skipped, symlinked files
/// included. Directory symlinks are followed (#1505/#1510, 9433419a) and a
/// realpath `visited` set keeps cycles and duplicate links from recursing
/// twice. Traversal failures are collected by the caller through the
/// diagnostic variant.
#[allow(dead_code)] // upstream API surface; exercised in tests
pub fn list_files_recursive(dir: &Path, predicate: fn(&str) -> bool) -> Vec<PathBuf> {
    let mut diagnostics = Vec::new();
    collect_files_recursive(dir, predicate, AgentSource::User, &mut diagnostics)
}

/// Internal walker behind [`list_files_recursive`]: returns matching files in
/// traversal order and pushes one [`DiscoverDiagnostic`] per unreadable
/// entry/directory (C-R3). A missing root is silent (upstream
/// `listFilesRecursive` returns `[]` for `!existsSync(dir)`).
fn collect_files_recursive(
    dir: &Path,
    predicate: fn(&str) -> bool,
    scope: AgentSource,
    diagnostics: &mut Vec<DiscoverDiagnostic>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.exists() {
        return files;
    }
    let mut visited = HashSet::new();
    walk_discovery_dir(
        dir,
        dir,
        predicate,
        scope,
        &mut visited,
        &mut files,
        diagnostics,
    );
    files
}

fn walk_discovery_dir(
    root_dir: &Path,
    dir: &Path,
    predicate: fn(&str) -> bool,
    scope: AgentSource,
    visited: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    diagnostics: &mut Vec<DiscoverDiagnostic>,
) {
    // Upstream marks the directory itself visited before iterating
    // (agents.ts:1702-1707); `cycle -> .` is therefore skipped on the first
    // hop. canonicalize is the realpathSync equivalent.
    match std::fs::canonicalize(dir) {
        Ok(real) => {
            if !visited.insert(real) {
                return;
            }
        }
        Err(error) => {
            diagnostics.push(DiscoverDiagnostic {
                path: dir.to_path_buf(),
                scope,
                error: format!("cannot resolve directory: {error}"),
            });
            return;
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(DiscoverDiagnostic {
                path: dir.to_path_buf(),
                scope,
                error: format!("cannot read directory: {error}"),
            });
            return;
        }
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|entry| entry.file_name());
    for entry in sorted {
        let file_path = dir.join(entry.file_name());
        let name = entry.file_name().to_string_lossy().to_string();
        // `fs::metadata` follows symlinks (#1505/#1510): a symlink to a
        // directory is walked as a directory, a symlink to a file still
        // passes the name predicate (C-R4).
        let metadata = match std::fs::metadata(&file_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                diagnostics.push(DiscoverDiagnostic {
                    path: file_path,
                    scope,
                    error: format!("cannot read entry metadata: {error}"),
                });
                continue;
            }
        };
        if metadata.is_dir() {
            if !should_prune_discovery_dir(root_dir, &file_path, &name) {
                walk_discovery_dir(
                    root_dir,
                    &file_path,
                    predicate,
                    scope,
                    visited,
                    files,
                    diagnostics,
                );
            }
            continue;
        }
        if metadata.is_file() && predicate(&name) {
            files.push(file_path);
        }
    }
}

/// `isLegacyAgentSkillPath` (agents.ts:1426-1433): a `.agents/skills` segment
/// inside the discovery tree is the legacy skills area, not agent definitions.
fn is_legacy_agent_skill_path(root_dir: &Path, file_path: &Path) -> bool {
    let root_is_agents = root_dir
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case(".agents"))
        .unwrap_or(false);
    let mut parts: Vec<String> = file_path
        .strip_prefix(root_dir)
        .unwrap_or(file_path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    if root_is_agents {
        parts.insert(0, ".agents".to_string());
    }
    parts
        .windows(2)
        .any(|w| w[0] == ".agents" && w[1] == "skills")
}

/// `findProjectRootCandidates` + `findConfiguredProjectRoot`
/// (agents.ts:629-669). P0 always resolves `nearest` — `projectRootResolution`
/// is a P1 settings key (requirements §3.2), so the git-root policy branch is
/// not implemented (deviation TE-D15 scope note).
pub fn find_configured_project_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        if is_project_root_candidate(&current) {
            return Some(current);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

fn user_agent_dirs() -> Vec<PathBuf> {
    // extra dirs (PATH-style) → `<agentDir>/agents` (old) → `~/.agents` (new)
    // (agents.ts:1726-1744). Order within the user level is preserved;
    // `mergeAgentsForScope` consumes only the merged user set.
    let mut dirs = Vec::new();
    if let Ok(raw) = std::env::var(EXTRA_AGENT_DIRS_ENV) {
        for part in std::env::split_paths(&raw) {
            if !part.as_os_str().is_empty() {
                dirs.push(part);
            }
        }
    }
    dirs.push(paths::get_agent_dir().join("agents"));
    if let Some(home) = paths::home_dir() {
        dirs.push(home.join(".agents"));
    }
    dirs
}

fn project_agent_dirs(cwd: &Path) -> Vec<PathBuf> {
    // agents.ts:1698-1712: legacy `<root>/.agents` first, then preferred
    // `<root>/.rpi/agents`; both are read, preferred wins on same name via
    // load order (later entries overwrite in the per-source map below).
    let Some(root) = find_configured_project_root(cwd) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let legacy = root.join(".agents");
    if legacy.is_dir() {
        dirs.push(legacy);
    }
    let preferred = paths::get_project_config_dir(&root).join("agents");
    if preferred.is_dir() {
        dirs.push(preferred);
    }
    dirs
}

/// `discoverAgents` (agents.ts:1742-1796) P0 subset: no package level, no
/// modelScope. Returns merged agents (disabled filtered) per scope rules.
/// Fatal frontmatter errors propagate like the upstream throw.
pub fn discover_agents(
    cwd: &Path,
    scope: &str,
    settings: &crate::config::SettingsPair,
    builtin_dir: Option<&Path>,
) -> Result<Vec<AgentConfig>, String> {
    Ok(discover_agents_with_diagnostics(cwd, scope, settings, builtin_dir).0)
}

/// Additive discovery entry point (R7.1.3.1): same discovery as
/// [`discover_agents`] plus one [`DiscoverDiagnostic`] per skipped definition
/// or unreadable directory, in discovery order (builtin → user dirs → project
/// dirs). Per-file failures never abort the batch.
pub fn discover_agents_with_diagnostics(
    cwd: &Path,
    scope: &str,
    settings: &crate::config::SettingsPair,
    builtin_dir: Option<&Path>,
) -> (Vec<AgentConfig>, Vec<DiscoverDiagnostic>) {
    discover_agents_with_user_dirs_with_diagnostics(
        cwd,
        scope,
        settings,
        builtin_dir,
        user_agent_dirs(),
    )
}

/// Test seam over [`discover_agents`]: explicit user-level directories
/// (extra dirs → agentDir/agents → ~/.agents upstream order).
#[allow(dead_code)] // backward-compatible API surface (A-R2); exercised in tests
pub fn discover_agents_with_user_dirs(
    cwd: &Path,
    scope: &str,
    settings: &crate::config::SettingsPair,
    builtin_dir: Option<&Path>,
    user_dirs: Vec<PathBuf>,
) -> Result<Vec<AgentConfig>, String> {
    Ok(discover_agents_with_user_dirs_with_diagnostics(
        cwd,
        scope,
        settings,
        builtin_dir,
        user_dirs,
    )
    .0)
}

/// Diagnostics-aware variant of [`discover_agents_with_user_dirs`].
pub fn discover_agents_with_user_dirs_with_diagnostics(
    cwd: &Path,
    scope: &str,
    settings: &crate::config::SettingsPair,
    builtin_dir: Option<&Path>,
    user_dirs: Vec<PathBuf>,
) -> (Vec<AgentConfig>, Vec<DiscoverDiagnostic>) {
    let default_model = settings.default_model.clone();
    // `applySubagentDefaults` (agents.ts:995-1009, called per scope at
    // 1760/1771/1779): defaultModel → defaultThinking → defaultExtensions,
    // each fill-only.
    let default_thinking = settings.default_thinking.clone();
    let default_extensions = settings.default_extensions.clone();
    let mut diagnostics = Vec::new();

    let (mut builtin, mut builtin_diagnostics) =
        crate::agents::builtin::load_builtin_agents_with_diagnostics(builtin_dir);
    diagnostics.append(&mut builtin_diagnostics);
    apply_subagent_defaults(
        &mut builtin,
        &default_model,
        &default_thinking,
        &default_extensions,
    );
    apply_builtin_overrides(&mut builtin, settings);

    let mut user: Vec<AgentConfig> = if scope == "project" {
        Vec::new()
    } else {
        let mut agents = Vec::new();
        for dir in user_dirs {
            let (mut dir_agents, mut dir_diagnostics) =
                load_agents_from_dir_with_diagnostics(&dir, "user");
            agents.append(&mut dir_agents);
            diagnostics.append(&mut dir_diagnostics);
        }
        // Same-source dedupe: first definition wins (agents.ts:1844-1849).
        dedupe_by_name(agents)
    };
    apply_default_model(&mut user, &default_model);
    apply_default_thinking(&mut user, &default_thinking);
    apply_default_extensions(&mut user, &default_extensions);
    apply_custom_overrides(
        &mut user,
        &settings.project.overrides,
        &settings.user.overrides,
    );

    let mut project: Vec<AgentConfig> = if scope == "user" {
        Vec::new()
    } else {
        let mut agents = Vec::new();
        for dir in project_agent_dirs(cwd) {
            let (mut dir_agents, mut dir_diagnostics) =
                load_agents_from_dir_with_diagnostics(&dir, "project");
            agents.append(&mut dir_agents);
            diagnostics.append(&mut dir_diagnostics);
        }
        dedupe_by_name(agents)
    };
    apply_default_model(&mut project, &default_model);
    apply_default_thinking(&mut project, &default_thinking);
    apply_default_extensions(&mut project, &default_extensions);
    apply_custom_overrides(
        &mut project,
        &settings.project.overrides,
        &settings.user.overrides,
    );

    // mergeAgentsForScope (agent-selection.ts:3-25): map insertion order
    // builtin → user → project; within "user"/"project" scopes only that
    // level is inserted after the builtins.
    let mut merged: BTreeMap<String, AgentConfig> = BTreeMap::new();
    for agent in builtin {
        merged.insert(agent.name.clone(), agent);
    }
    if scope == "both" || scope == "user" {
        for agent in user {
            merged.insert(agent.name.clone(), agent);
        }
    }
    if scope == "both" || scope == "project" {
        for agent in project {
            merged.insert(agent.name.clone(), agent);
        }
    }
    (
        merged
            .into_values()
            .filter(|agent| agent.disabled != Some(true))
            .collect(),
        diagnostics,
    )
}

fn dedupe_by_name(agents: Vec<AgentConfig>) -> Vec<AgentConfig> {
    let mut seen = std::collections::BTreeSet::new();
    agents
        .into_iter()
        .filter(|agent| seen.insert(agent.name.clone()))
        .collect()
}

/// `applySubagentDefaults` model fill (agents.ts:955-993): only agents
/// without an explicit model inherit `subagents.defaultModel`.
fn apply_default_model(agents: &mut [AgentConfig], default_model: &Option<String>) {
    if let Some(default_model) = default_model {
        for agent in agents.iter_mut() {
            if agent.model.is_none() {
                agent.model = Some(default_model.clone());
            }
        }
    }
}

/// `applySubagentDefaults` composition (agents.ts:995-1009).
fn apply_subagent_defaults(
    agents: &mut [AgentConfig],
    default_model: &Option<String>,
    default_thinking: &Option<String>,
    default_extensions: &Option<Vec<String>>,
) {
    apply_default_model(agents, default_model);
    apply_default_thinking(agents, default_thinking);
    apply_default_extensions(agents, default_extensions);
}

/// `applySubagentDefaultThinking` (agents.ts:995-1009): fill only agents
/// without a frontmatter thinking level.
fn apply_default_thinking(agents: &mut [AgentConfig], default_thinking: &Option<String>) {
    let Some(default_thinking) = default_thinking else {
        return;
    };
    for agent in agents.iter_mut() {
        if matches!(agent.thinking, ThinkingSpec::Unset) {
            agent.thinking = ThinkingSpec::Level(default_thinking.clone());
        }
    }
}

/// `applySubagentDefaultExtensions` (agents.ts:1011-1019): fill only agents
/// that did not declare `extensions`.
fn apply_default_extensions(agents: &mut [AgentConfig], default_extensions: &Option<Vec<String>>) {
    let Some(default_extensions) = default_extensions else {
        return;
    };
    for agent in agents.iter_mut() {
        if agent.extensions.is_none() {
            agent.extensions = Some(default_extensions.clone());
        }
    }
}

/// `applyBuiltinOverrides` (agents.ts:1051-1104): project override → project
/// bulk disable → user override → user bulk disable; disableBuiltins replaces
/// the entry with `{disabled: true}` (upstream masks the other scope).
/// `disableThinking` clears the thinking level of builtin agents unless the
/// winning override entry sets an explicit `thinking` (applyGlobalThinking,
/// agents.ts:1066-1069).
fn apply_builtin_overrides(agents: &mut [AgentConfig], settings: &crate::config::SettingsPair) {
    let disable_thinking = settings.disable_thinking;
    let clear_thinking = |agent: &mut AgentConfig, explicit_override: bool| {
        if disable_thinking && !explicit_override {
            agent.thinking = ThinkingSpec::Unset;
        }
    };
    for agent in agents.iter_mut() {
        if let Some(project_override) = settings.project.overrides.get(&agent.name) {
            let explicit = project_override.thinking.is_some();
            apply_override_entry(agent, project_override);
            clear_thinking(agent, explicit);
            continue;
        }
        if settings.project_bulk_disabled {
            agent.disabled = Some(true);
            clear_thinking(agent, false);
            continue;
        }
        if let Some(user_override) = settings.user.overrides.get(&agent.name) {
            // agents.ts:1085: an explicit user-override thinking protects from
            // clearing only when the project file does not configure
            // disableThinking.
            let explicit =
                !settings.project_thinking_configured && user_override.thinking.is_some();
            apply_override_entry(agent, user_override);
            clear_thinking(agent, explicit);
            continue;
        }
        if settings.user_bulk_disabled {
            agent.disabled = Some(true);
        }
        clear_thinking(agent, false);
    }
}

/// `applyCustomAgentOverride` (agents.ts:1111-1206): fill-only semantics — a
/// field applies only when the frontmatter did not declare it (description and
/// disabled are always applicable; disabled only when currently unset).
/// Project override wins over user (agents.ts:1210-1228).
fn apply_custom_overrides(
    agents: &mut [AgentConfig],
    project_overrides: &BTreeMap<String, crate::config::AgentOverride>,
    user_overrides: &BTreeMap<String, crate::config::AgentOverride>,
) {
    for agent in agents.iter_mut() {
        if let Some(project_override) = project_overrides.get(&agent.name) {
            apply_custom_override_entry(agent, project_override);
            continue;
        }
        if let Some(user_override) = user_overrides.get(&agent.name) {
            apply_custom_override_entry(agent, user_override);
        }
    }
}

fn apply_override_entry(agent: &mut AgentConfig, entry: &crate::config::AgentOverride) {
    // Builtin overrides replace wholesale (project/user settings win over the
    // shipped definition) — `applyBuiltinOverride` (agents.ts:1011-1043).
    if let Some(description) = &entry.description {
        agent.description = description.clone();
    }
    if let Some(model) = &entry.model {
        agent.model = model.clone();
    }
    if let Some(disabled) = entry.disabled {
        agent.disabled = Some(disabled);
    }
    if let Some(tools) = &entry.tools {
        let (split, mcp) = split_tool_list(tools.clone());
        agent.tools = split;
        agent.mcp_direct_tools = mcp;
    }
    if let Some(fallback_models) = &entry.fallback_models {
        agent.fallback_models = fallback_models.clone().unwrap_or_default();
    }
    if let Some(thinking) = &entry.thinking {
        agent.thinking = match thinking {
            Some(level) => ThinkingSpec::Level(level.clone()),
            None => ThinkingSpec::Unset,
        };
    }
    if let Some(mode) = &entry.system_prompt_mode {
        agent.system_prompt_mode = match mode.as_str() {
            "append" => "append",
            _ => "replace",
        };
    }
    if let Some(inherit) = entry.inherit_project_context {
        agent.inherit_project_context = inherit;
    }
    if let Some(inherit) = entry.inherit_skills {
        agent.inherit_skills = inherit;
    }
    if let Some(default_context) = &entry.default_context {
        agent.default_context = match default_context.as_deref() {
            Some("fork") => Some(ContextMode::Fork),
            Some("fresh") => Some(ContextMode::Fresh),
            _ => None,
        };
    }
    if let Some(role) = &entry.acceptance_role {
        agent.acceptance_role = role.clone();
    }
    if let Some(system_prompt) = &entry.system_prompt {
        agent.system_prompt = system_prompt.clone();
    }
    if let Some(skills) = &entry.skills {
        agent.skills = skills.clone().unwrap_or_default();
    }
}

fn apply_custom_override_entry(agent: &mut AgentConfig, entry: &crate::config::AgentOverride) {
    // `applyCustomAgentOverride` (agents.ts:1123-1206): fill-only via
    // frontmatter field presence; `false` clears (delete upstream).
    if let Some(description) = &entry.description {
        agent.description = description.clone();
    }
    if let Some(model) = &entry.model {
        if !agent.has_frontmatter_field(&["model"]) {
            agent.model = model.clone();
        }
    }
    if let Some(disabled) = entry.disabled {
        if agent.disabled.is_none() {
            agent.disabled = Some(disabled);
        }
    }
    if let Some(tools) = &entry.tools {
        if !agent.has_frontmatter_field(&["tools"]) {
            let (split, mcp) = split_tool_list(tools.clone());
            agent.tools = split;
            agent.mcp_direct_tools = mcp;
        }
    }
    if let Some(fallback_models) = &entry.fallback_models {
        if !agent.has_frontmatter_field(&["fallbackModels"]) {
            agent.fallback_models = fallback_models.clone().unwrap_or_default();
        }
    }
    if let Some(thinking) = &entry.thinking {
        if !agent.has_frontmatter_field(&["thinking"]) {
            agent.thinking = match thinking {
                Some(level) => ThinkingSpec::Level(level.clone()),
                None => ThinkingSpec::Unset,
            };
        }
    }
    if let Some(mode) = &entry.system_prompt_mode {
        if !agent.has_frontmatter_field(&["systemPromptMode"]) {
            agent.system_prompt_mode = match mode.as_str() {
                "append" => "append",
                _ => "replace",
            };
        }
    }
    if let Some(inherit) = entry.inherit_project_context {
        if !agent.has_frontmatter_field(&["inheritProjectContext"]) {
            agent.inherit_project_context = inherit;
        }
    }
    if let Some(inherit) = entry.inherit_skills {
        if !agent.has_frontmatter_field(&["inheritSkills"]) {
            agent.inherit_skills = inherit;
        }
    }
    if let Some(default_context) = &entry.default_context {
        if !agent.has_frontmatter_field(&["defaultContext"]) {
            agent.default_context = match default_context.as_deref() {
                Some("fork") => Some(ContextMode::Fork),
                Some("fresh") => Some(ContextMode::Fresh),
                _ => None,
            };
        }
    }
    if let Some(role) = &entry.acceptance_role {
        if !agent.has_frontmatter_field(&["acceptanceRole"]) {
            agent.acceptance_role = role.clone();
        }
    }
    if let Some(skills) = &entry.skills {
        if !agent.has_frontmatter_field(&["skill", "skills"]) {
            agent.skills = skills.clone().unwrap_or_default();
        }
    }
}

/// `resolveAgentName` + `effectiveAgentMatch` (agents.ts:501-529).
pub fn resolve_agent_name<'a>(
    agents: &'a [AgentConfig],
    raw: &str,
) -> Result<Option<&'a AgentConfig>, String> {
    let exact: Vec<&AgentConfig> = agents
        .iter()
        .filter(|a| a.name == raw || a.local_name == raw)
        .collect();
    if !exact.is_empty() {
        return finish_agent_match(exact, raw, "name");
    }
    let by_alias: Vec<&AgentConfig> = agents
        .iter()
        .filter(|a| {
            a.aliases
                .as_deref()
                .is_some_and(|aliases| aliases.iter().any(|x| x == raw))
        })
        .collect();
    if !by_alias.is_empty() {
        return finish_agent_match(by_alias, raw, "alias");
    }
    Ok(None)
}

fn finish_agent_match<'a>(
    matches: Vec<&'a AgentConfig>,
    raw: &str,
    kind: &str,
) -> Result<Option<&'a AgentConfig>, String> {
    let distinct: std::collections::BTreeSet<&str> =
        matches.iter().map(|a| a.name.as_str()).collect();
    if distinct.len() == 1 {
        // Same runtime name from multiple sources: highest source rank wins
        // (project > user > builtin).
        let best = matches
            .into_iter()
            .max_by_key(|a| a.source)
            .expect("matches is non-empty");
        Ok(Some(best))
    } else {
        let names: Vec<&str> = distinct.into_iter().collect();
        Err(format!(
            "Ambiguous agent {kind} '{raw}': {}",
            names.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_agent(dir: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, format!("---\n{frontmatter}\n---\n{body}")).unwrap();
        path
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rpi-sub-disc-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn package_name_normalization() {
        assert_eq!(
            parse_package_name(Some("My Pkg")).unwrap(),
            Some("my-pkg".to_string())
        );
        assert_eq!(
            parse_package_name(Some("a.b.c")).unwrap(),
            Some("a.b.c".to_string())
        );
        assert_eq!(parse_package_name(Some("")).unwrap(), None);
        assert!(parse_package_name(Some("!!!")).is_err());
        assert_eq!(parse_package_name(Some("false")).unwrap(), None);
    }

    #[test]
    fn agent_from_content_defaults_follow_name() {
        let agent = agent_from_content(
            "---\nname: delegate\ndescription: d\n---\nbody",
            Path::new("/x/delegate.md"),
            AgentSource::Builtin,
        )
        .unwrap()
        .unwrap();
        assert_eq!(agent.system_prompt_mode, "append");
        assert!(agent.inherit_project_context);
        assert!(!agent.inherit_skills);

        let custom = agent_from_content(
            "---\nname: mine\ndescription: d\n---\nbody",
            Path::new("/x/mine.md"),
            AgentSource::User,
        )
        .unwrap()
        .unwrap();
        assert_eq!(custom.system_prompt_mode, "replace");
        assert!(!custom.inherit_project_context);
    }

    #[test]
    fn invalid_async_is_fatal_like_upstream() {
        let result = agent_from_content(
            "---\nname: x\ndescription: d\nasync: maybe\n---\nb",
            Path::new("/x/x.md"),
            AgentSource::User,
        );
        assert!(result.unwrap_err().contains("invalid async frontmatter"));
    }

    #[test]
    fn chain_files_are_excluded_and_mcp_split() {
        let dir = temp_root("chain");
        write_agent(
            &dir,
            "a",
            "name: a\ndescription: d\ntools: read, mcp:srv.tool",
            "b",
        );
        write_agent(&dir, "b.chain", "name: b\ndescription: d", "b");
        let agents = load_agents_from_dir(&dir, "user").unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].tools, Some(vec!["read".to_string()]));
        assert_eq!(agents[0].mcp_direct_tools, vec!["srv.tool".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_override_and_scope() {
        let root = temp_root("ovr");
        let project = root.join("proj");
        let user = root.join("user");
        std::fs::create_dir_all(project.join(".rpi")).unwrap();
        // Same local name in user and project: project wins.
        write_agent(
            &user.join("agents"),
            "scout",
            "name: scout\ndescription: user scout",
            "u",
        );
        write_agent(
            &project.join(".rpi").join("agents"),
            "scout",
            "name: scout\ndescription: project scout",
            "p",
        );
        let settings = crate::config::SettingsPair::default();
        let user_dirs = vec![user.join("agents")];
        let found_user_scope =
            discover_agents_with_user_dirs(&project, "user", &settings, None, user_dirs.clone())
                .unwrap();
        // The user definition overrides the builtin scout (same name), so the
        // total stays at six.
        assert_eq!(found_user_scope.len(), 6);
        assert_eq!(
            found_user_scope
                .iter()
                .find(|a| a.name == "scout")
                .unwrap()
                .description,
            "user scout"
        );
        let found_both =
            discover_agents_with_user_dirs(&project, "both", &settings, None, user_dirs).unwrap();
        let scout = found_both.iter().find(|a| a.name == "scout").unwrap();
        assert_eq!(scout.description, "project scout");
        assert_eq!(scout.source, AgentSource::Project);
        let found_project_only =
            discover_agents_with_user_dirs(&project, "project", &settings, None, vec![]).unwrap();
        // Builtins stay in the map (mergeAgentsForScope always seeds them);
        // the project scope only suppresses the user level.
        assert!(found_project_only
            .iter()
            .all(|a| a.source != AgentSource::User));
        assert_eq!(
            found_project_only
                .iter()
                .find(|a| a.name == "scout")
                .unwrap()
                .source,
            AgentSource::Project
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn alias_resolution_prefers_exact_and_reports_ambiguous() {
        let root = temp_root("alias");
        write_agent(
            &root,
            "one",
            "name: one\ndescription: d\naliases: helper",
            "b",
        );
        write_agent(
            &root,
            "two",
            "name: two\ndescription: d\naliases: helper",
            "b",
        );
        let agents = load_agents_from_dir(&root, "user").unwrap();
        let exact = resolve_agent_name(&agents, "one").unwrap().unwrap();
        assert_eq!(exact.name, "one");
        let err = resolve_agent_name(&agents, "helper").unwrap_err();
        assert!(
            err.contains("Ambiguous agent alias 'helper': one, two"),
            "{err}"
        );
        assert!(resolve_agent_name(&agents, "nope").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn builtin_disable_via_settings() {
        let settings = crate::config::SettingsPair {
            user_bulk_disabled: true,
            ..Default::default()
        };
        let found = discover_agents_with_user_dirs(
            Path::new("/nonexistent"),
            "both",
            &settings,
            None,
            vec![],
        )
        .unwrap();
        assert!(found.iter().all(|a| a.source != AgentSource::Builtin));
        assert!(found.is_empty());
    }

    #[test]
    fn builtin_override_tools_and_default_model_fill() {
        let mut user_settings = crate::config::SubagentSettings {
            default_model: Some("model-x".to_string()),
            ..Default::default()
        };
        user_settings.overrides.insert(
            "researcher".to_string(),
            crate::config::AgentOverride {
                tools: Some(Some(vec!["read".to_string(), "write".to_string()])),
                ..Default::default()
            },
        );
        let settings = crate::config::SettingsPair {
            user: user_settings,
            default_model: Some("model-x".to_string()),
            ..Default::default()
        };
        let found = discover_agents_with_user_dirs(
            Path::new("/nonexistent"),
            "both",
            &settings,
            None,
            vec![],
        )
        .unwrap();
        let researcher = found.iter().find(|a| a.name == "researcher").unwrap();
        // Override replaces the builtin web-tool allowlist entirely.
        assert_eq!(
            researcher.tools,
            Some(vec!["read".to_string(), "write".to_string()])
        );
        assert_eq!(researcher.mcp_direct_tools.len(), 0);
        // Builtins without an explicit model inherit subagents.defaultModel.
        assert_eq!(researcher.model.as_deref(), Some("model-x"));
    }
}

/// TE15 discovery robustness tests (R7.1.3.1–.4). The fixture tree is the
/// TE13 deliverable `fixtures/subagents-v066/discovery/agents-tree/`;
/// `materialize.json` in the same directory pins what the upstream v0.66.0
/// snapshot produces for it (silent skips, diagnostics, pruned paths).
#[cfg(test)]
mod discovery_robustness_tests {
    use super::*;
    use std::time::Duration;

    const HIDDEN_RPI_AGENT: &str = "---\nname: hidden-rpi-agent\ndescription: pruned\n---\nbody\n";

    fn fixture_tree() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/subagents-v066/discovery/agents-tree")
    }

    fn copy_dir_recursive(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir_recursive(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }

    /// Materialize `agents-tree/` plus the `materialize.json` extras (`.rpi/`,
    /// symlinks) into a fresh sandbox. Returns `(sandbox_root, tree_root)`.
    fn materialize(tag: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("rpi-sub-disc15-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tree = root.join("agents");
        copy_dir_recursive(&fixture_tree(), &tree);
        std::fs::create_dir_all(tree.join(".rpi")).unwrap();
        std::fs::write(tree.join(".rpi").join("pruned.md"), HIDDEN_RPI_AGENT).unwrap();
        std::fs::create_dir_all(root.join("cwd")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("nested", tree.join("linked-nested")).unwrap();
            std::os::unix::fs::symlink(".", tree.join("cycle")).unwrap();
        }
        (root, tree)
    }

    fn discover_fixture(tree: &Path) -> (Vec<AgentConfig>, Vec<DiscoverDiagnostic>) {
        let cwd = tree.parent().unwrap().join("cwd");
        discover_agents_with_user_dirs_with_diagnostics(
            &cwd,
            "both",
            &crate::config::SettingsPair::default(),
            None,
            vec![tree.to_path_buf()],
        )
    }

    fn names(agents: &[AgentConfig]) -> Vec<String> {
        let mut names: Vec<String> = agents.iter().map(|agent| agent.name.clone()).collect();
        names.sort();
        names
    }

    /// T-1 (A2/A4): a fatal per-file error is isolated, every other agent
    /// (including the builtins) stays visible and the diagnostic carries
    /// path/scope/error without file contents.
    #[test]
    fn bad_file_is_isolated_and_builtins_stay_visible() {
        let (root, tree) = materialize("t1");
        let (agents, diagnostics) = discover_fixture(&tree);

        let visible = names(&agents);
        for expected in [
            "delegate",
            "oracle",
            "researcher",
            "reviewer",
            "scout",
            "worker",
            "good-agent",
            "good-nested",
        ] {
            assert!(
                visible.iter().any(|name| name == expected),
                "{expected} missing: {visible:?}"
            );
        }
        assert!(!visible.iter().any(|name| name == "broken-async"));
        assert!(!visible.iter().any(|name| name == "broken-timeout"));

        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert_eq!(diagnostics[0].path, tree.join("broken-invalid-async.md"));
        assert_eq!(diagnostics[0].scope, AgentSource::User);
        assert_eq!(
            diagnostics[0].error,
            "Agent 'broken-async' has invalid async frontmatter; expected true or false."
        );
        assert_eq!(diagnostics[1].path, tree.join("broken-invalid-timeout.md"));
        assert_eq!(
            diagnostics[1].error,
            "Agent 'broken-timeout' has invalid timeoutMs frontmatter; expected a positive integer."
        );
        for diagnostic in &diagnostics {
            assert!(
                !diagnostic.error.contains("This file exists only"),
                "diagnostic must not embed file contents: {diagnostic:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-2: one diagnostic per fatal file, in stable traversal order.
    #[test]
    fn multiple_bad_files_yield_one_diagnostic_each_in_order() {
        let root =
            std::env::temp_dir().join(format!("rpi-sub-disc15-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("a-bad.md"),
            "---\nname: a\ndescription: d\nasync: nope\n---\nb\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b-good.md"),
            "---\nname: b\ndescription: d\n---\nb\n",
        )
        .unwrap();
        std::fs::write(
            root.join("c-bad.md"),
            "---\nname: c\ndescription: d\ntimeoutMs: -1\n---\nb\n",
        )
        .unwrap();
        let (agents, diagnostics) = load_agents_from_dir_with_diagnostics(&root, "user");
        assert_eq!(names(&agents), vec!["b".to_string()]);
        let paths: Vec<PathBuf> = diagnostics.iter().map(|d| d.path.clone()).collect();
        assert_eq!(paths, vec![root.join("a-bad.md"), root.join("c-bad.md")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-3 (B-R2/B-R3): `.rpi` and `sync-backups` are pruned at any depth, but
    /// an explicit discovery root inside `.rpi` is still read.
    #[test]
    fn nested_pruned_dirs_are_skipped_and_explicit_root_is_read() {
        let (root, tree) = materialize("t3");
        let (agents, diagnostics) = load_agents_from_dir_with_diagnostics(&tree, "user");
        let visible = names(&agents);
        assert!(visible.contains(&"good-agent".to_string()), "{visible:?}");
        assert!(
            !visible.contains(&"hidden-rpi-agent".to_string()),
            "{visible:?}"
        );
        assert!(
            !visible.contains(&"sync-backup-agent".to_string()),
            "{visible:?}"
        );
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");

        let explicit_root = root.join("proj/.rpi/agents");
        std::fs::create_dir_all(&explicit_root).unwrap();
        std::fs::write(
            explicit_root.join("inside.md"),
            "---\nname: inside-agent\ndescription: d\n---\nb\n",
        )
        .unwrap();
        let (inside, inside_diagnostics) =
            load_agents_from_dir_with_diagnostics(&explicit_root, "project");
        assert_eq!(names(&inside), vec!["inside-agent".to_string()]);
        assert!(inside_diagnostics.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-4 (C-R1/C-R2): a symlinked directory is followed and the realpath
    /// visited set resolves its agents exactly once.
    #[cfg(unix)]
    #[test]
    fn symlinked_directory_is_followed_once() {
        let (root, tree) = materialize("t4");
        let (agents, _) = load_agents_from_dir_with_diagnostics(&tree, "user");
        let nested: Vec<&AgentConfig> = agents
            .iter()
            .filter(|agent| agent.name == "good-nested")
            .collect();
        assert_eq!(nested.len(), 1, "{agents:?}");
        // Traversal order is name-sorted, so `linked-nested` is entered before
        // `nested` and owns the file path (upstream parity).
        assert_eq!(
            nested[0].file_path,
            tree.join("linked-nested/good-nested.md")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-5 (A3/C-R2): a symlink cycle terminates within the guard window and
    /// does not duplicate agents.
    #[cfg(unix)]
    #[test]
    fn symlink_cycle_terminates_without_duplicates() {
        let (root, tree) = materialize("t5");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (agents, diagnostics) = load_agents_from_dir_with_diagnostics(&tree, "user");
            let _ = sender.send((names(&agents), diagnostics.len()));
        });
        let (visible, diagnostics) = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("discovery must terminate despite the A->B->A symlink cycle");
        assert_eq!(
            visible
                .iter()
                .filter(|name| name.as_str() == "good-agent")
                .count(),
            1,
            "{visible:?}"
        );
        assert_eq!(diagnostics, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-6 (C-R3): broken links and unreadable directories become diagnostics
    /// and never abort the walk or panic.
    #[cfg(unix)]
    #[test]
    fn broken_link_and_permission_failures_are_diagnosed() {
        let root = std::env::temp_dir().join(format!("rpi-sub-disc15-t6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("good.md"),
            "---\nname: good\ndescription: d\n---\nb\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("missing-target", root.join("dangling.md")).unwrap();
        let (agents, diagnostics) = load_agents_from_dir_with_diagnostics(&root, "user");
        assert_eq!(names(&agents), vec!["good".to_string()]);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].path, root.join("dangling.md"));
        assert!(
            diagnostics[0].error.contains("cannot read entry metadata"),
            "{diagnostics:?}"
        );

        // Permission failure: chmod 000 a subdirectory. When the process can
        // still read it (root), the assertion is skipped.
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(
            locked.join("hidden.md"),
            "---\nname: hidden\ndescription: d\n---\nb\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let readable = std::fs::read_dir(&locked).is_ok();
        let (agents, diagnostics) = load_agents_from_dir_with_diagnostics(&root, "user");
        assert_eq!(names(&agents), vec!["good".to_string()]);
        if !readable {
            assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
            assert!(diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == locked
                    && diagnostic.error.contains("cannot read directory")));
        }
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-7 (D-R3): user/project override priority is unchanged by the
    /// diagnostics/ prunning rework.
    #[test]
    fn multi_level_override_priority_is_unchanged() {
        let root = std::env::temp_dir().join(format!("rpi-sub-disc15-t7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user/agents");
        let project = root.join("proj/.rpi/agents");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            user.join("scout.md"),
            "---\nname: scout\ndescription: user scout\n---\nu\n",
        )
        .unwrap();
        std::fs::write(
            project.join("scout.md"),
            "---\nname: scout\ndescription: project scout\n---\np\n",
        )
        .unwrap();
        let cwd = root.join("proj");
        let (agents, diagnostics) = discover_agents_with_user_dirs_with_diagnostics(
            &cwd,
            "both",
            &crate::config::SettingsPair::default(),
            None,
            vec![user.clone()],
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let scout = agents.iter().find(|agent| agent.name == "scout").unwrap();
        assert_eq!(scout.description, "project scout");
        assert_eq!(scout.source, AgentSource::Project);
        let (user_scope, _) = discover_agents_with_user_dirs_with_diagnostics(
            &cwd,
            "user",
            &crate::config::SettingsPair::default(),
            None,
            vec![user],
        );
        assert_eq!(
            user_scope
                .iter()
                .find(|agent| agent.name == "scout")
                .unwrap()
                .description,
            "user scout"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-8 (D-R2): `list`/`get` shapes stay byte-identical when no diagnostics
    /// exist; the diagnostic block is strictly additive.
    #[test]
    fn list_output_shape_is_unchanged_and_diagnostics_append() {
        let (root, tree) = materialize("t8");
        let (agents, diagnostics) = discover_fixture(&tree);
        let base = crate::actions::format_agent_list(&agents);
        let block = crate::actions::format_discovery_diagnostics(&diagnostics);
        assert_eq!(block.len(), 3, "{block:?}");
        assert_eq!(block[0], "Invalid agent definitions:");
        assert!(block[1].contains("broken-invalid-async.md"));
        assert!(block[1].contains("(user): Agent 'broken-async'"));
        let combined = format!("{base}\n{}", block.join("\n"));
        assert!(combined.starts_with(&base), "existing lines must not move");
        assert!(combined.ends_with(&block.join("\n")));

        let good = agents
            .iter()
            .find(|agent| agent.name == "good-agent")
            .unwrap();
        let detail = crate::actions::format_agent_detail(good);
        assert!(detail.starts_with("Agent: good-agent (user)"), "{detail}");
        assert!(detail.contains("Path: "));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-9 (A1): the legacy wrappers keep their signature and return exactly
    /// the diagnostics-aware agent list.
    #[test]
    fn legacy_wrappers_return_the_same_agents() {
        let (root, tree) = materialize("t9");
        let (new_agents, _) = load_agents_from_dir_with_diagnostics(&tree, "user");
        let legacy_agents = load_agents_from_dir(&tree, "user").unwrap();
        assert_eq!(names(&legacy_agents), names(&new_agents));
        assert_eq!(
            legacy_agents
                .iter()
                .map(|agent| agent.file_path.clone())
                .collect::<Vec<_>>(),
            new_agents
                .iter()
                .map(|agent| agent.file_path.clone())
                .collect::<Vec<_>>()
        );

        let cwd = root.join("cwd");
        let settings = crate::config::SettingsPair::default();
        let legacy =
            discover_agents_with_user_dirs(&cwd, "both", &settings, None, vec![tree.clone()])
                .unwrap();
        let (new, _) = discover_agents_with_user_dirs_with_diagnostics(
            &cwd,
            "both",
            &settings,
            None,
            vec![tree.clone()],
        );
        assert_eq!(names(&legacy), names(&new));
        // Public entry point keeps the Result shape (A-R2) and resolves the
        // ambient user dirs through the same wrapper.
        let from_public = discover_agents(&cwd, "both", &settings, None).unwrap();
        let ambient =
            discover_agents_with_user_dirs(&cwd, "both", &settings, None, user_agent_dirs())
                .unwrap();
        assert_eq!(names(&from_public), names(&ambient));
        let _ = std::fs::remove_dir_all(&root);
    }
}
