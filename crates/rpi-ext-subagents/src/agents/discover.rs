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

use std::collections::BTreeMap;
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

/// Frontmatter → AgentConfig (`loadAgentsFromDir` body, agents.ts:1510-1656).
/// `Err` mirrors the upstream throws (invalid async/timeoutMs abort the whole
/// discovery); `Ok(None)` = file skipped (missing name/description, invalid
/// package) exactly like upstream `continue`.
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
        Err(_) => return Ok(None),
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
        frontmatter_fields: fm.keys().cloned().collect(),
    }))
}

/// Load agents from one directory (`loadAgentsFromDir`, agents.ts:1497-1662).
/// Malformed definitions that upstream treats as fatal (invalid async /
/// timeoutMs) abort the whole directory load with the upstream message.
pub fn load_agents_from_dir(dir: &Path, source: &str) -> Result<Vec<AgentConfig>, String> {
    let source = match source {
        "builtin" => AgentSource::Builtin,
        "user" => AgentSource::User,
        "project" => AgentSource::Project,
        _ => AgentSource::User,
    };
    let mut agents = Vec::new();
    for file_path in list_files_recursive(dir, root_predicate()) {
        if is_legacy_agent_skill_path(dir, &file_path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file_path) else {
            continue;
        };
        if let Some(agent) = agent_from_content(&content, &file_path, source)? {
            agents.push(agent);
        }
    }
    Ok(agents)
}

fn root_predicate() -> fn(&str) -> bool {
    // `.md` but not `.chain.md` (agents.ts:1500).
    |file_name: &str| file_name.ends_with(".md") && !file_name.ends_with(".chain.md")
}

const DISCOVERY_PRUNED_DIR_NAMES: [&str; 2] = [".git", "node_modules"];

fn should_prune_discovery_dir(root_dir: &Path, dir: &Path, dir_name: &str) -> bool {
    // agents.ts:1394-1398.
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

/// `listFilesRecursive` (agents.ts:1400-1424): name-sorted (byte order ≈
/// localeCompare for ASCII), depth-first, pruned dirs skipped, symlinked files
/// included.
pub fn list_files_recursive(dir: &Path, predicate: fn(&str) -> bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|entry| entry.file_name());
    for entry in sorted {
        let file_path = dir.join(entry.file_name());
        let meta = entry.file_type().ok();
        if matches!(meta, Some(t) if t.is_dir()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !should_prune_discovery_dir(dir, &file_path, &name) {
                files.extend(list_files_recursive(&file_path, predicate));
            }
            continue;
        }
        // isFile || isSymbolicLink: entries whose type is a symlink to a file
        // pass the predicate by name.
        let name = entry.file_name().to_string_lossy().to_string();
        if predicate(&name) {
            files.push(file_path);
        }
    }
    files
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
    discover_agents_with_user_dirs(cwd, scope, settings, builtin_dir, user_agent_dirs())
}

/// Test seam over [`discover_agents`]: explicit user-level directories
/// (extra dirs → agentDir/agents → ~/.agents upstream order).
pub fn discover_agents_with_user_dirs(
    cwd: &Path,
    scope: &str,
    settings: &crate::config::SettingsPair,
    builtin_dir: Option<&Path>,
    user_dirs: Vec<PathBuf>,
) -> Result<Vec<AgentConfig>, String> {
    let default_model = settings.default_model.clone();

    let mut builtin = crate::agents::builtin::load_builtin_agents(builtin_dir);
    apply_default_model(&mut builtin, &default_model);
    apply_builtin_overrides(&mut builtin, settings);

    let mut user: Vec<AgentConfig> = if scope == "project" {
        Vec::new()
    } else {
        let mut agents = Vec::new();
        for dir in user_dirs {
            agents.extend(load_agents_from_dir(&dir, "user")?);
        }
        // Same-source dedupe: first definition wins (agents.ts:1844-1849).
        dedupe_by_name(agents)
    };
    apply_default_model(&mut user, &default_model);
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
            agents.extend(load_agents_from_dir(&dir, "project")?);
        }
        dedupe_by_name(agents)
    };
    apply_default_model(&mut project, &default_model);
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
    Ok(merged
        .into_values()
        .filter(|agent| agent.disabled != Some(true))
        .collect())
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

/// `applyBuiltinOverrides` (agents.ts:1051-1104): project override → project
/// bulk disable → user override → user bulk disable; disableBuiltins replaces
/// the entry with `{disabled: true}` (upstream masks the other scope).
fn apply_builtin_overrides(agents: &mut [AgentConfig], settings: &crate::config::SettingsPair) {
    for agent in agents.iter_mut() {
        if let Some(project_override) = settings.project.overrides.get(&agent.name) {
            apply_override_entry(agent, project_override);
            continue;
        }
        if settings.project_bulk_disabled {
            agent.disabled = Some(true);
            continue;
        }
        if let Some(user_override) = settings.user.overrides.get(&agent.name) {
            apply_override_entry(agent, user_override);
            continue;
        }
        if settings.user_bulk_disabled {
            agent.disabled = Some(true);
        }
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
    // shipped definition).
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
}

fn apply_custom_override_entry(agent: &mut AgentConfig, entry: &crate::config::AgentOverride) {
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
