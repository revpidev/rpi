//! Port of the skills subsystem @ pi 0.82.1 (2efa728):
//! - `packages/coding-agent/src/core/skills.ts` (full file: frontmatter
//!   parsing and validation, `loadSkillsFromDir` / `loadSkillFromFile`,
//!   `loadSkills` dedupe, `formatSkillsForPrompt`)
//! - the skills-discovery parts of
//!   `packages/coding-agent/src/core/package-manager.ts`:
//!   `resourcePrecedenceRank` (:184-188), the pattern helpers
//!   (`isPattern` / `splitPatterns` :271-299, `matchesAnyPattern` /
//!   `matchesAnyExactPattern` / `applyPatterns` / `isEnabledByOverrides`
//!   :644-718, :728-774), `collectSkillEntries` (:349-421),
//!   `findGitRepoRoot` / `collectAncestorAgentsSkillDirs` (:427-460),
//!   `resolveLocalEntries` (:2280-2301), the skills branches of
//!   `addAutoDiscoveredResources` (:2303-2467) and the
//!   `toResolvedPaths` rank-sort / canonical dedupe (:2527-2545)
//! - `_expandSkillCommand` of `core/agent-session.ts:1301-1325`
//! - the read-tool injection gate of `core/system-prompt.ts:97-101,155-157`
//!   (exposed here as a function parameter)
//!
//! Intentional differences:
//! - `.pi` → `.pir`, `PI_` → `PIR_` (ADR-0001). The upstream discovery mode
//!   `"pi"` is [`SkillDiscoveryMode::Pir`] here.
//! - Directory walking uses the `ignore` crate's [`WalkBuilder`]
//!   (per-directory `.gitignore` / `.ignore` / `.fdignore` chains with real
//!   gitignore semantics, hidden-entry skip, symlink following) instead of
//!   upstream's hand-rolled `readdirSync` recursion with a prefix-rewritten
//!   `ignore`-package matcher (skills.ts:24-65, package-manager.ts:228-269).
//!   Behavior matches for anchored patterns; an unanchored pattern in a
//!   nested ignore file (e.g. `foo` in `sub/.gitignore`) matches at any
//!   depth below `sub/` here, but only `sub/foo` itself upstream.
//! - Settings/CLI glob matching uses a small built-in matcher supporting
//!   `*`, `?` and `**`; upstream uses `minimatch`
//!   (package-manager.ts:644-695). Brace expansion (`{a,b}`) and character
//!   classes (`[abc]`) are not supported.
//! - `format_skills_for_prompt` takes the read-tool-active flag as a
//!   parameter; upstream the gate lives inline in system-prompt.ts
//!   (`tools.includes("read")` / `selectedTools.includes("read")`).
//! - `expand_skill_command` returns `Err(PirError::Resource)` when the skill
//!   file cannot be read; upstream emits the error through the extension
//!   runner and returns the original text (agent-session.ts:1316-1324).
//!   Callers should fall back to the original text on `Err`.
//! - Package resources (npm/git sources) are out of scope for this slice;
//!   [`SourceOrigin::Package`] is kept so [`resource_precedence_rank`] stays
//!   faithful (package-manager.ts:184-188).
//! - Settings `skills` entries containing `*`/`?` are treated purely as
//!   enablement filters over files collected from the plain entries, exactly
//!   like upstream `resolveLocalEntries` (they are never globbed against the
//!   filesystem there).
//! - Name/description length limits count Unicode scalar values; upstream
//!   counts UTF-16 code units (JS `string.length`). Identical for BMP text.
//! - Non-string YAML values for `name` / `description` are treated as
//!   absent; upstream would coerce or throw depending on the value.
//! - Diagnostics and errors: validation/collision findings are returned as
//!   structured [`ResourceDiagnostic`] values (never `Err`); only
//!   `/skill:name` expansion surfaces a [`PirError`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::config;
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

/// `MAX_NAME_LENGTH` (skills.ts:11).
const MAX_NAME_LENGTH: usize = 64;
/// `MAX_DESCRIPTION_LENGTH` (skills.ts:14).
const MAX_DESCRIPTION_LENGTH: usize = 1024;
/// Ignore file names honored during discovery
/// (`IGNORE_FILE_NAMES`, skills.ts:16 / package-manager.ts:209).
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

// ---------------------------------------------------------------------------
// Diagnostics (diagnostics.ts)
// ---------------------------------------------------------------------------

/// `ResourceDiagnostic["type"]` (diagnostics.ts:11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Error,
    Collision,
}

/// `ResourceCollision["resourceType"]` (diagnostics.ts:2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticResourceType {
    Extension,
    Skill,
    Prompt,
    Theme,
}

/// `ResourceCollision` (diagnostics.ts:1-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCollision {
    pub resource_type: DiagnosticResourceType,
    pub name: String,
    pub winner_path: PathBuf,
    pub loser_path: PathBuf,
    pub winner_source: Option<String>,
    pub loser_source: Option<String>,
}

/// `ResourceDiagnostic` (diagnostics.ts:10-15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    pub path: Option<PathBuf>,
    pub collision: Option<ResourceCollision>,
}

fn warning(message: impl Into<String>, path: &Path) -> ResourceDiagnostic {
    ResourceDiagnostic {
        kind: DiagnosticKind::Warning,
        message: message.into(),
        path: Some(path.to_path_buf()),
        collision: None,
    }
}

// ---------------------------------------------------------------------------
// Source metadata (source-info.ts + package-manager.ts PathMetadata)
// ---------------------------------------------------------------------------

/// `SourceScope` (source-info.ts:3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SourceScope {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "project")]
    Project,
    #[serde(rename = "temporary")]
    Temporary,
}

/// `SourceOrigin` (source-info.ts:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SourceOrigin {
    #[serde(rename = "package")]
    Package,
    #[serde(rename = "top-level")]
    TopLevel,
}

/// `PathMetadata["source"]` for non-package resources
/// (package-manager.ts): `"local"` (settings/CLI entry) or `"auto"`
/// (auto-discovered).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataSource {
    Local,
    Auto,
}

/// `PathMetadata` (package-manager.ts) — skills-relevant subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMetadata {
    pub source: MetadataSource,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<PathBuf>,
}

/// `resourcePrecedenceRank` (package-manager.ts:184-188). Lower rank =
/// higher precedence: project settings (0) > project auto (1) > user
/// settings (2) > user auto (3) > package (4).
pub fn resource_precedence_rank(metadata: &PathMetadata) -> u8 {
    if metadata.origin == SourceOrigin::Package {
        return 4;
    }
    let scope_base = if metadata.scope == SourceScope::Project {
        0
    } else {
        2
    };
    scope_base
        + if metadata.source == MetadataSource::Local {
            0
        } else {
            1
        }
}

/// `SourceInfo` (source-info.ts:6-12).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: PathBuf,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Skill + frontmatter (skills.ts:67-81, utils/frontmatter.ts)
// ---------------------------------------------------------------------------

/// `Skill` (skills.ts:74-81).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
}

/// `LoadSkillsResult` (skills.ts:83-86).
#[derive(Debug, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// The `source` string threaded through upstream `loadSkillsFromDir` /
/// `loadSkillFromFile` (skills.ts:129-158): `"user"`, `"project"` or
/// `"path"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLoadSource {
    User,
    Project,
    Path,
}

/// `createSkillSourceInfo` (skills.ts:136-158): synthetic `SourceInfo` with
/// `source: "local"`, `origin: "top-level"` and the scope implied by the
/// load source (`"path"` → `temporary` via `createSyntheticSourceInfo`
/// defaults, source-info.ts:24-33).
fn create_skill_source_info(
    file_path: &Path,
    base_dir: &Path,
    source: SkillLoadSource,
) -> SourceInfo {
    let scope = match source {
        SkillLoadSource::User => SourceScope::User,
        SkillLoadSource::Project => SourceScope::Project,
        SkillLoadSource::Path => SourceScope::Temporary,
    };
    SourceInfo {
        path: file_path.to_path_buf(),
        source: "local".to_string(),
        scope,
        origin: SourceOrigin::TopLevel,
        base_dir: Some(base_dir.to_path_buf()),
    }
}

/// `SkillFrontmatter` (skills.ts:67-72). Remaining spec fields are parsed
/// (as part of the YAML value) but ignored.
#[derive(Debug, Default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    disable_model_invocation: bool,
}

/// `extractFrontmatter` (frontmatter.ts:10-26): normalize newlines, then
/// split a leading `---` block. Returns `(yaml, body)`; the body is trimmed
/// only when a frontmatter block was found.
fn extract_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    // JS: normalized.indexOf("\n---", 3)
    let Some(end) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return (None, normalized);
    };
    // JS: normalized.slice(4, endIndex) — empty when endIndex < 4.
    let yaml = if end >= 4 {
        normalized[4..end].to_string()
    } else {
        String::new()
    };
    // JS: normalized.slice(endIndex + 4).trim()
    let body = normalized[end + 4..].trim().to_string();
    (Some(yaml), body)
}

/// `parseFrontmatter` (frontmatter.ts:28-37) specialized to
/// [`SkillFrontmatter`]. A YAML document that is not a mapping behaves like
/// upstream's unchecked cast: all fields read as absent.
fn parse_frontmatter(content: &str) -> Result<(SkillFrontmatter, String), serde_yaml::Error> {
    let (yaml, body) = extract_frontmatter(content);
    let Some(yaml) = yaml else {
        return Ok((SkillFrontmatter::default(), body));
    };
    let value: serde_yaml::Value = serde_yaml::from_str(&yaml)?;
    let mut frontmatter = SkillFrontmatter::default();
    if let serde_yaml::Value::Mapping(mapping) = value {
        let get = |key: &str| mapping.get(serde_yaml::Value::String(key.to_string()));
        frontmatter.name = get("name").and_then(|v| v.as_str()).map(str::to_string);
        frontmatter.description = get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        frontmatter.disable_model_invocation = get("disable-model-invocation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    Ok((frontmatter, body))
}

/// `stripFrontmatter` (frontmatter.ts:39).
pub fn strip_frontmatter(content: &str) -> String {
    extract_frontmatter(content).1
}

// ---------------------------------------------------------------------------
// Validation (skills.ts:92-127)
// ---------------------------------------------------------------------------

/// `validateName` (skills.ts:92-112). Violations are warnings only — the
/// skill still loads.
fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();

    let len = name.chars().count();
    if len > MAX_NAME_LENGTH {
        errors.push(format!("name exceeds {MAX_NAME_LENGTH} characters ({len})"));
    }

    // /^[a-z0-9-]+$/
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }

    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }

    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }

    errors
}

/// `validateDescription` (skills.ts:117-127).
fn validate_description(description: Option<&str>) -> Vec<String> {
    match description {
        None => vec!["description is required".to_string()],
        Some(d) if d.trim().is_empty() => vec!["description is required".to_string()],
        Some(d) if d.chars().count() > MAX_DESCRIPTION_LENGTH => vec![format!(
            "description exceeds {} characters ({})",
            MAX_DESCRIPTION_LENGTH,
            d.chars().count()
        )],
        Some(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Directory walking (skills.ts:160-275 + package-manager.ts:347-425)
// ---------------------------------------------------------------------------

/// `SkillDiscoveryMode` (package-manager.ts:347). Upstream mode `"pi"` is
/// `Pir` (ADR-0001 rename): loose `.md` files at the scan root count as
/// skills. Mode `"agents"` only recognizes `SKILL.md` directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiscoveryMode {
    Pir,
    Agents,
}

/// `collectSkillEntries` (package-manager.ts:349-421), also covering
/// `loadSkillsFromDirInternal`'s walk (skills.ts:173-275):
///
/// - a directory containing `SKILL.md` is a skill root — its `SKILL.md` is
///   the only entry and recursion stops there
/// - otherwise, in [`SkillDiscoveryMode::Pir`] mode, loose `.md` files at
///   the scan root count as skills
/// - dot-directories/dot-files and `node_modules` are skipped
/// - `.gitignore` / `.ignore` / `.fdignore` chains are honored at every
///   level (even outside a git repository)
/// - symlinks are followed; broken links are skipped
pub fn collect_skill_entries(dir: &Path, mode: SkillDiscoveryMode) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }

    let mut builder = WalkBuilder::new(dir);
    builder
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false)
        .follow_links(true)
        .add_custom_ignore_filename(IGNORE_FILE_NAMES[2])
        .filter_entry(|entry| entry.file_name() != "node_modules");

    let mut skill_mds = Vec::new();
    let mut root_loose = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            // Broken symlinks and unreadable entries are skipped upstream
            // (skills.ts:204-207, 243-246).
            Err(_) => continue,
        };
        let path = entry.path();
        if path == dir {
            continue;
        }
        let is_file = entry.file_type().is_some_and(|t| t.is_file());
        if !is_file {
            continue;
        }
        if entry.file_name() == "SKILL.md" {
            skill_mds.push(path.to_path_buf());
            continue;
        }
        // `mode === "pi" && dir === root` (package-manager.ts:406-409).
        if mode == SkillDiscoveryMode::Pir
            && path.parent() == Some(dir)
            && entry.file_name().to_string_lossy().ends_with(".md")
        {
            root_loose.push(path.to_path_buf());
        }
    }

    // A directory that contains SKILL.md is a skill root: keep only the
    // shallowest SKILL.md files and drop everything beneath a skill root
    // (upstream never recurses past one — it `return`s on the first pass).
    //
    // Sort by (depth, path): upstream emits raw `readdirSync` order, which is
    // filesystem-dependent and therefore not a portable contract — the pinned
    // goldens capture alphabetical-within-depth order (git checkout order on
    // the generation machine). Sorting the same way makes the parity
    // deterministic on any filesystem.
    skill_mds.sort_by(|a, b| {
        (a.components().count(), a.as_os_str()).cmp(&(b.components().count(), b.as_os_str()))
    });
    let mut skill_roots: Vec<PathBuf> = Vec::new();
    for path in skill_mds {
        let Some(parent) = path.parent() else {
            continue;
        };
        if skill_roots.iter().any(|root| parent.starts_with(root)) {
            continue;
        }
        skill_roots.push(parent.to_path_buf());
        entries.push(path);
    }

    // Loose root `.md` files are only collected when the scan root itself
    // is not a skill root (upstream's first pass would have returned).
    let root_is_skill_root = entries.iter().any(|p| p.parent() == Some(dir));
    if !root_is_skill_root {
        entries.extend(root_loose);
    }

    entries
}

/// `loadSkillsFromDir` (skills.ts:168-171): scan `dir` with root-loose-`.md`
/// handling (the `includeRootFiles: true` walk) and load every entry.
pub fn load_skills_from_dir(dir: &Path, source: SkillLoadSource) -> LoadSkillsResult {
    let mut result = LoadSkillsResult::default();
    if !dir.exists() {
        return result;
    }
    for path in collect_skill_entries(dir, SkillDiscoveryMode::Pir) {
        let (skill, diagnostics) = load_skill_from_file(&path, source);
        if let Some(skill) = skill {
            result.skills.push(skill);
        }
        result.diagnostics.extend(diagnostics);
    }
    result
}

/// `loadSkillFromFile` (skills.ts:277-325): parse + validate one skill
/// file. A missing/empty description drops the skill (with a warning);
/// every other violation is a warning and the skill still loads.
fn load_skill_from_file(
    file_path: &Path,
    source: SkillLoadSource,
) -> (Option<Skill>, Vec<ResourceDiagnostic>) {
    let mut diagnostics = Vec::new();

    let raw = match std::fs::read_to_string(file_path) {
        Ok(raw) => raw,
        Err(error) => {
            diagnostics.push(warning(error.to_string(), file_path));
            return (None, diagnostics);
        }
    };
    let (frontmatter, _body) = match parse_frontmatter(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            diagnostics.push(warning(error.to_string(), file_path));
            return (None, diagnostics);
        }
    };

    let skill_dir = file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let parent_dir_name = skill_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Description errors are reported before name errors (skills.ts:290-302).
    for error in validate_description(frontmatter.description.as_deref()) {
        diagnostics.push(warning(error, file_path));
    }

    // `frontmatter.name || parentDirName` (skills.ts:296).
    let name = match frontmatter.name {
        Some(name) if !name.is_empty() => name,
        _ => parent_dir_name,
    };
    for error in validate_name(&name) {
        diagnostics.push(warning(error, file_path));
    }

    // Still load with warnings — unless the description is missing entirely
    // (skills.ts:305-307).
    let Some(description) = frontmatter.description.filter(|d| !d.trim().is_empty()) else {
        return (None, diagnostics);
    };

    let skill = Skill {
        name,
        description,
        file_path: file_path.to_path_buf(),
        base_dir: skill_dir.clone(),
        source_info: create_skill_source_info(file_path, &skill_dir, source),
        disable_model_invocation: frontmatter.disable_model_invocation,
    };
    (Some(skill), diagnostics)
}

// ---------------------------------------------------------------------------
// Ancestor .agents/skills scan (package-manager.ts:427-460)
// ---------------------------------------------------------------------------

/// `findGitRepoRoot` (package-manager.ts:427-439): nearest ancestor
/// containing `.git` (file or directory), `None` at the filesystem root.
pub fn find_git_repo_root(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = resolve_against_cwd(start_dir);
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// `collectAncestorAgentsSkillDirs` (package-manager.ts:441-460): every
/// `{dir}/.agents/skills` from `start_dir` upward, stopping after the git
/// repo root (or at the filesystem root when there is no repository).
pub fn collect_ancestor_agents_skill_dirs(start_dir: &Path) -> Vec<PathBuf> {
    let mut skill_dirs = Vec::new();
    let resolved = resolve_against_cwd(start_dir);
    let git_repo_root = find_git_repo_root(&resolved);

    let mut dir = resolved;
    loop {
        skill_dirs.push(dir.join(config::AGENTS_DIR_NAME).join("skills"));
        if git_repo_root.as_ref() == Some(&dir) {
            break;
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent.to_path_buf();
    }
    skill_dirs
}

/// Node `resolve()` for an input that is already absolute in practice;
/// relative inputs resolve against the process cwd (upstream default).
fn resolve_against_cwd(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    resolve_path(&path.to_string_lossy(), &cwd)
}

// ---------------------------------------------------------------------------
// Settings pattern filtering (package-manager.ts:271-299, 644-774)
// ---------------------------------------------------------------------------

/// `isPattern` (package-manager.ts:271-273).
fn is_pattern(s: &str) -> bool {
    s.starts_with('!')
        || s.starts_with('+')
        || s.starts_with('-')
        || s.contains('*')
        || s.contains('?')
}

/// `splitPatterns` (package-manager.ts:283-299).
fn split_patterns(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut patterns = Vec::new();
    for entry in entries {
        if is_pattern(entry) {
            patterns.push(entry.clone());
        } else {
            plain.push(entry.clone());
        }
    }
    (plain, patterns)
}

/// `getOverridePatterns` (package-manager.ts:697-699).
fn get_override_patterns(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|p| p.starts_with('!') || p.starts_with('+') || p.starts_with('-'))
        .cloned()
        .collect()
}

/// Minimal glob matcher replacing `minimatch` (see module docs): `*`
/// matches within a path segment, `**` across segments (including zero
/// segments before a `/`), `?` a single non-separator character.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(pattern: &[char], text: &[char]) -> bool {
        let Some((&head, rest)) = pattern.split_first() else {
            return text.is_empty();
        };
        match head {
            '*' if rest.first() == Some(&'*') => {
                let rest = &rest[1..];
                if rest.is_empty() {
                    return true;
                }
                for i in 0..=text.len() {
                    if inner(rest, &text[i..]) {
                        return true;
                    }
                    // `a/**/b` also matches `a/b` (zero segments).
                    if rest[0] == '/' && inner(&rest[1..], &text[i..]) {
                        return true;
                    }
                }
                false
            }
            '*' => {
                for i in 0..=text.len() {
                    if i > 0 && text[i - 1] == '/' {
                        return false;
                    }
                    if inner(rest, &text[i..]) {
                        return true;
                    }
                }
                false
            }
            '?' => !text.is_empty() && text[0] != '/' && inner(rest, &text[1..]),
            c => !text.is_empty() && text[0] == c && inner(rest, &text[1..]),
        }
    }
    inner(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

/// `toPosixPath` (skills.ts:20-22).
fn to_posix_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Node `path.relative` for lexical (already-resolved) inputs.
fn lexical_relative(base: &Path, path: &Path) -> PathBuf {
    let base_components: Vec<_> = base.components().collect();
    let path_components: Vec<_> = path.components().collect();
    let mut common = 0;
    while common < base_components.len()
        && common < path_components.len()
        && base_components[common] == path_components[common]
    {
        common += 1;
    }
    let mut relative = PathBuf::new();
    for _ in common..base_components.len() {
        relative.push("..");
    }
    for component in &path_components[common..] {
        relative.push(component);
    }
    relative
}

/// The match candidate strings of `matchesAnyPattern` /
/// `matchesAnyExactPattern` (package-manager.ts:644-652, 677-685). For
/// `SKILL.md` files the parent directory variants also participate.
struct MatchCandidates {
    rel: String,
    name: String,
    abs: String,
    parent_rel: Option<String>,
    parent_name: Option<String>,
    parent_abs: Option<String>,
}

fn match_candidates(file_path: &Path, base_dir: &Path) -> MatchCandidates {
    let name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (parent_rel, parent_name, parent_abs) = if name == "SKILL.md" {
        let parent = file_path.parent().map(Path::to_path_buf);
        (
            parent
                .as_ref()
                .map(|p| to_posix_path(&lexical_relative(base_dir, p))),
            parent
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned()),
            parent.as_ref().map(|p| to_posix_path(p)),
        )
    } else {
        (None, None, None)
    };
    MatchCandidates {
        rel: to_posix_path(&lexical_relative(base_dir, file_path)),
        name,
        abs: to_posix_path(file_path),
        parent_rel,
        parent_name,
        parent_abs,
    }
}

/// `matchesAnyPattern` (package-manager.ts:654-670).
fn matches_any_pattern(candidates: &MatchCandidates, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.replace(std::path::MAIN_SEPARATOR, "/");
        if glob_match(&pattern, &candidates.rel)
            || glob_match(&pattern, &candidates.name)
            || glob_match(&pattern, &candidates.abs)
        {
            return true;
        }
        // `if (!isSkillFile) return false;` — parent variants are None for
        // non-SKILL.md files.
        candidates
            .parent_rel
            .as_ref()
            .is_some_and(|p| glob_match(&pattern, p))
            || candidates
                .parent_name
                .as_ref()
                .is_some_and(|p| glob_match(&pattern, p))
            || candidates
                .parent_abs
                .as_ref()
                .is_some_and(|p| glob_match(&pattern, p))
    })
}

/// `normalizeExactPattern` (package-manager.ts:672-675).
fn normalize_exact_pattern(pattern: &str) -> String {
    let normalized = pattern
        .strip_prefix("./")
        .or_else(|| pattern.strip_prefix(".\\"))
        .unwrap_or(pattern);
    normalized.replace(std::path::MAIN_SEPARATOR, "/")
}

/// `matchesAnyExactPattern` (package-manager.ts:687-695).
fn matches_any_exact_pattern(candidates: &MatchCandidates, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    patterns.iter().any(|pattern| {
        let normalized = normalize_exact_pattern(pattern);
        normalized == candidates.rel
            || normalized == candidates.abs
            || candidates.parent_rel.as_ref() == Some(&normalized)
            || candidates.parent_abs.as_ref() == Some(&normalized)
    })
}

/// `applyPatterns` (package-manager.ts:728-774): plain patterns include,
/// `!pattern` excludes, `+path` force-includes an exact path, `-path`
/// force-excludes (winning over force-includes).
pub fn apply_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> HashSet<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    for pattern in patterns {
        if let Some(rest) = pattern.strip_prefix('+') {
            force_includes.push(rest.to_string());
        } else if let Some(rest) = pattern.strip_prefix('-') {
            force_excludes.push(rest.to_string());
        } else if let Some(rest) = pattern.strip_prefix('!') {
            excludes.push(rest.to_string());
        } else {
            includes.push(pattern.clone());
        }
    }

    // Step 1: includes (or all when there are none).
    let mut result: Vec<PathBuf> = if includes.is_empty() {
        all_paths.to_vec()
    } else {
        all_paths
            .iter()
            .filter(|path| matches_any_pattern(&match_candidates(path, base_dir), &includes))
            .cloned()
            .collect()
    };
    // Step 2: excludes.
    if !excludes.is_empty() {
        result.retain(|path| !matches_any_pattern(&match_candidates(path, base_dir), &excludes));
    }
    // Step 3: force-include (adds back from allPaths, overriding exclusions).
    if !force_includes.is_empty() {
        for path in all_paths {
            if !result.contains(path)
                && matches_any_exact_pattern(&match_candidates(path, base_dir), &force_includes)
            {
                result.push(path.clone());
            }
        }
    }
    // Step 4: force-exclude (wins over everything).
    if !force_excludes.is_empty() {
        result.retain(|path| {
            !matches_any_exact_pattern(&match_candidates(path, base_dir), &force_excludes)
        });
    }
    result.into_iter().collect()
}

/// `isEnabledByOverrides` (package-manager.ts:701-718): enabled unless a
/// `!pattern` matches; `+path` re-enables, `-path` disables again (exact
/// matches only).
pub fn is_enabled_by_overrides(file_path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    let overrides = get_override_patterns(patterns);
    let excludes: Vec<String> = overrides
        .iter()
        .filter_map(|p| p.strip_prefix('!'))
        .map(str::to_string)
        .collect();
    let force_includes: Vec<String> = overrides
        .iter()
        .filter_map(|p| p.strip_prefix('+'))
        .map(str::to_string)
        .collect();
    let force_excludes: Vec<String> = overrides
        .iter()
        .filter_map(|p| p.strip_prefix('-'))
        .map(str::to_string)
        .collect();

    let candidates = match_candidates(file_path, base_dir);
    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(&candidates, &excludes) {
        enabled = false;
    }
    if !force_includes.is_empty() && matches_any_exact_pattern(&candidates, &force_includes) {
        enabled = true;
    }
    if !force_excludes.is_empty() && matches_any_exact_pattern(&candidates, &force_excludes) {
        enabled = false;
    }
    enabled
}

// ---------------------------------------------------------------------------
// Discovery (package-manager.ts:2280-2301, 2303-2467, 2527-2545)
// ---------------------------------------------------------------------------

/// One resolved skill path with its enablement state and provenance
/// (upstream `ResolvedResource` for skills).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillPath {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

/// Inputs for [`discover_skill_paths`]. `agent_dir` and `home_dir` are
/// passed explicitly (instead of read from the environment) so tests can
/// point them at temporary directories; [`DiscoverSkillsOptions::from_config`]
/// fills them from [`crate::config`].
#[derive(Debug, Clone)]
pub struct DiscoverSkillsOptions {
    /// Working directory (project scope base).
    pub cwd: PathBuf,
    /// Agent config directory (`~/.pir/agent` by default).
    pub agent_dir: PathBuf,
    /// User home directory; `~/.agents/skills` is skipped when `None`.
    pub home_dir: Option<PathBuf>,
    /// Trust gate for project-local resources (`.pir/skills` and ancestor
    /// `.agents/skills` are only discovered when trusted).
    pub project_trusted: bool,
    /// `settings.skills` from the global settings file.
    pub global_settings_skills: Vec<String>,
    /// `settings.skills` from the project settings file.
    pub project_settings_skills: Vec<String>,
    /// `--skills` CLI paths (resolved against `cwd`; appended after all
    /// discovered paths — resource-loader.ts:421 merge order).
    pub cli_skill_paths: Vec<String>,
}

impl DiscoverSkillsOptions {
    /// Fill `agent_dir` / `home_dir` from [`config::get_agent_dir`] and
    /// [`config::user_home_dir`].
    pub fn from_config(
        cwd: PathBuf,
        project_trusted: bool,
        global_settings_skills: Vec<String>,
        project_settings_skills: Vec<String>,
        cli_skill_paths: Vec<String>,
    ) -> Self {
        Self {
            cwd,
            agent_dir: config::get_agent_dir(),
            home_dir: config::user_home_dir(),
            project_trusted,
            global_settings_skills,
            project_settings_skills,
            cli_skill_paths,
        }
    }
}

/// `resolveLocalEntries` (package-manager.ts:2280-2301) for skills: plain
/// entries resolve against `base_dir` (files taken as-is, directories
/// scanned in [`SkillDiscoveryMode::Pir`]); pattern entries only filter
/// enablement via [`apply_patterns`].
pub fn resolve_local_skill_entries(
    entries: &[String],
    base_dir: &Path,
    metadata: &PathMetadata,
) -> Vec<ResolvedSkillPath> {
    if entries.is_empty() {
        return Vec::new();
    }
    let (plain, patterns) = split_patterns(entries);
    // `collectFilesFromPaths` (package-manager.ts:2469-2486) with the
    // skills branch of `collectResourceFiles` (:634-642).
    let mut all_files = Vec::new();
    for entry in &plain {
        let resolved = resolve_path(entry.trim(), base_dir);
        if resolved.is_file() {
            all_files.push(resolved);
        } else if resolved.is_dir() {
            all_files.extend(collect_skill_entries(&resolved, SkillDiscoveryMode::Pir));
        }
    }
    let enabled = apply_patterns(&all_files, &patterns, base_dir);
    all_files
        .into_iter()
        .map(|path| ResolvedSkillPath {
            enabled: enabled.contains(&path),
            path,
            metadata: metadata.clone(),
        })
        .collect()
}

/// The skills subset of `PackageManager.resolve` (package-manager.ts:901-953)
/// plus `addAutoDiscoveredResources` (:2303-2467) and `toResolvedPaths`
/// (:2527-2545): collect settings entries and auto-discovered paths, tag
/// them with precedence metadata, stable-sort by [`resource_precedence_rank`]
/// and dedupe by canonical path (first occurrence wins). CLI paths are
/// appended afterwards (they are not ranked upstream).
pub fn discover_skill_paths(options: &DiscoverSkillsOptions) -> Vec<ResolvedSkillPath> {
    let cwd = resolve_against_cwd(&options.cwd);
    let agent_dir = resolve_against_cwd(&options.agent_dir);
    let global_base_dir = agent_dir.clone();
    let project_base_dir = config::get_project_config_dir(&cwd);

    // Accumulator with `addResource` semantics (package-manager.ts:2506-2516):
    // insertion-ordered, first write for a given raw path wins.
    let mut accumulator: Vec<ResolvedSkillPath> = Vec::new();
    let mut seen_raw: HashSet<String> = HashSet::new();
    let mut add = |entry: ResolvedSkillPath| {
        if entry.path.as_os_str().is_empty() {
            return;
        }
        if seen_raw.insert(entry.path.to_string_lossy().into_owned()) {
            accumulator.push(entry);
        }
    };

    // Settings entries: project before global (package-manager.ts:922-948).
    let project_local = PathMetadata {
        source: MetadataSource::Local,
        scope: SourceScope::Project,
        origin: SourceOrigin::TopLevel,
        base_dir: None,
    };
    for entry in resolve_local_skill_entries(
        &options.project_settings_skills,
        &project_base_dir,
        &project_local,
    ) {
        add(entry);
    }
    let user_local = PathMetadata {
        source: MetadataSource::Local,
        scope: SourceScope::User,
        origin: SourceOrigin::TopLevel,
        base_dir: None,
    };
    for entry in resolve_local_skill_entries(
        &options.global_settings_skills,
        &global_base_dir,
        &user_local,
    ) {
        add(entry);
    }

    // Auto-discovered (package-manager.ts:2303-2467).
    if options.project_trusted {
        let metadata = PathMetadata {
            source: MetadataSource::Auto,
            scope: SourceScope::Project,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(project_base_dir.clone()),
        };
        for path in collect_skill_entries(
            &config::get_project_skills_dir(&cwd),
            SkillDiscoveryMode::Pir,
        ) {
            let enabled =
                is_enabled_by_overrides(&path, &options.project_settings_skills, &project_base_dir);
            add(ResolvedSkillPath {
                path,
                enabled,
                metadata: metadata.clone(),
            });
        }

        // Ancestor `.agents/skills` dirs (trust-gated), each with its own
        // baseDir (the `.agents` directory); `~/.agents/skills` is excluded
        // from this chain (package-manager.ts:2350-2352).
        let user_agents_skills_dir = options
            .home_dir
            .as_ref()
            .map(|home| home.join(config::AGENTS_DIR_NAME).join("skills"));
        for agents_skills_dir in collect_ancestor_agents_skill_dirs(&cwd) {
            if let Some(user_dir) = &user_agents_skills_dir {
                if resolve_against_cwd(&agents_skills_dir) == resolve_against_cwd(user_dir) {
                    continue;
                }
            }
            let agents_base_dir = agents_skills_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let metadata = PathMetadata {
                source: MetadataSource::Auto,
                scope: SourceScope::Project,
                origin: SourceOrigin::TopLevel,
                base_dir: Some(agents_base_dir.clone()),
            };
            for path in collect_skill_entries(&agents_skills_dir, SkillDiscoveryMode::Agents) {
                let enabled = is_enabled_by_overrides(
                    &path,
                    &options.project_settings_skills,
                    &agents_base_dir,
                );
                add(ResolvedSkillPath {
                    path,
                    enabled,
                    metadata: metadata.clone(),
                });
            }
        }
    }

    // User skills from `{agentDir}/skills` (`~/.pir/agent/skills`).
    let user_metadata = PathMetadata {
        source: MetadataSource::Auto,
        scope: SourceScope::User,
        origin: SourceOrigin::TopLevel,
        base_dir: Some(global_base_dir.clone()),
    };
    for path in collect_skill_entries(&agent_dir.join("skills"), SkillDiscoveryMode::Pir) {
        let enabled =
            is_enabled_by_overrides(&path, &options.global_settings_skills, &global_base_dir);
        add(ResolvedSkillPath {
            path,
            enabled,
            metadata: user_metadata.clone(),
        });
    }

    // User skills from `~/.agents/skills` (its own baseDir).
    if let Some(home_dir) = &options.home_dir {
        let user_agents_base_dir = home_dir.join(config::AGENTS_DIR_NAME);
        let metadata = PathMetadata {
            source: MetadataSource::Auto,
            scope: SourceScope::User,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(user_agents_base_dir.clone()),
        };
        for path in collect_skill_entries(
            &user_agents_base_dir.join("skills"),
            SkillDiscoveryMode::Agents,
        ) {
            let enabled = is_enabled_by_overrides(
                &path,
                &options.global_settings_skills,
                &user_agents_base_dir,
            );
            add(ResolvedSkillPath {
                path,
                enabled,
                metadata: metadata.clone(),
            });
        }
    }

    // `toResolvedPaths` (package-manager.ts:2527-2545): stable rank sort,
    // then canonical-path dedupe (first occurrence wins).
    let mut indexed: Vec<(usize, ResolvedSkillPath)> =
        accumulator.into_iter().enumerate().collect();
    indexed.sort_by_key(|(index, entry)| (resource_precedence_rank(&entry.metadata), *index));
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    let mut resolved: Vec<ResolvedSkillPath> = Vec::new();
    for (_, entry) in indexed {
        if seen_canonical.insert(canonicalize_path(&entry.path)) {
            resolved.push(entry);
        }
    }

    // `--skills` CLI paths come last (resource-loader.ts:421: discovered
    // enabled skills win name collisions over `additionalSkillPaths`).
    let cli_metadata = PathMetadata {
        source: MetadataSource::Local,
        scope: SourceScope::User,
        origin: SourceOrigin::TopLevel,
        base_dir: None,
    };
    for raw in &options.cli_skill_paths {
        let path = resolve_path(raw.trim(), &cwd);
        if seen_canonical.insert(canonicalize_path(&path)) {
            resolved.push(ResolvedSkillPath {
                path,
                enabled: true,
                metadata: cli_metadata.clone(),
            });
        }
    }

    resolved
}

// ---------------------------------------------------------------------------
// Loading (skills.ts:372-487)
// ---------------------------------------------------------------------------

/// `LoadSkillsOptions` (skills.ts:372-381).
#[derive(Debug, Clone)]
pub struct LoadSkillsOptions {
    /// Working directory for project-local skills.
    pub cwd: PathBuf,
    /// Agent config directory for global skills.
    pub agent_dir: PathBuf,
    /// Explicit skill paths (files or directories), already in precedence
    /// order — first occurrence of a name wins.
    pub skill_paths: Vec<PathBuf>,
    /// Also load `{agentDir}/skills` and `{cwd}/.pir/skills` first
    /// (`includeDefaults`; resource-loader passes `false` because discovery
    /// already covers those directories).
    pub include_defaults: bool,
}

/// `canonicalizePath` (paths.ts:28-34): realpath, falling back to the raw
/// path when the target does not resolve.
pub fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `isUnderPath` (skills.ts:438-445) for resolved inputs.
fn is_under_path(target: &Path, root: &Path) -> bool {
    target == root || target.starts_with(root)
}

/// `loadSkills` (skills.ts:387-487): load skills from the given paths with
/// real-path (symlink) dedupe and first-wins name-collision resolution.
/// Name collisions produce `collision` diagnostics; the winner is the first
/// path in `skill_paths` order.
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let cwd_base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolved_cwd = resolve_path(&options.cwd.to_string_lossy(), &cwd_base);
    let resolved_agent_dir = resolve_path(&options.agent_dir.to_string_lossy(), &cwd_base);

    let mut skills_by_name: HashMap<String, Skill> = HashMap::new();
    let mut name_order: Vec<String> = Vec::new();
    let mut real_paths: HashSet<PathBuf> = HashSet::new();
    let mut all_diagnostics: Vec<ResourceDiagnostic> = Vec::new();
    let mut collision_diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    // `addSkills` (skills.ts:399-428).
    fn add_skill(
        skill: Skill,
        skills_by_name: &mut HashMap<String, Skill>,
        name_order: &mut Vec<String>,
        real_paths: &mut HashSet<PathBuf>,
        collision_diagnostics: &mut Vec<ResourceDiagnostic>,
    ) {
        let real_path = canonicalize_path(&skill.file_path);
        // Already loaded this exact file (via symlink) — skip silently.
        if real_paths.contains(&real_path) {
            return;
        }
        if let Some(existing) = skills_by_name.get(&skill.name) {
            collision_diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("name \"{}\" collision", skill.name),
                path: Some(skill.file_path.clone()),
                collision: Some(ResourceCollision {
                    resource_type: DiagnosticResourceType::Skill,
                    name: skill.name.clone(),
                    winner_path: existing.file_path.clone(),
                    loser_path: skill.file_path.clone(),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            real_paths.insert(real_path);
            name_order.push(skill.name.clone());
            skills_by_name.insert(skill.name.clone(), skill);
        }
    }

    macro_rules! add_skills {
        ($result:expr) => {{
            let result: LoadSkillsResult = $result;
            all_diagnostics.extend(result.diagnostics);
            for skill in result.skills {
                add_skill(
                    skill,
                    &mut skills_by_name,
                    &mut name_order,
                    &mut real_paths,
                    &mut collision_diagnostics,
                );
            }
        }};
    }

    if options.include_defaults {
        add_skills!(load_skills_from_dir(
            &resolved_agent_dir.join("skills"),
            SkillLoadSource::User
        ));
        add_skills!(load_skills_from_dir(
            &config::get_project_skills_dir(&resolved_cwd),
            SkillLoadSource::Project
        ));
    }

    let user_skills_dir = resolved_agent_dir.join("skills");
    let project_skills_dir = config::get_project_skills_dir(&resolved_cwd);

    // `getSource` (skills.ts:447-453).
    let get_source = |resolved: &Path| -> SkillLoadSource {
        if !options.include_defaults {
            if is_under_path(resolved, &user_skills_dir) {
                return SkillLoadSource::User;
            }
            if is_under_path(resolved, &project_skills_dir) {
                return SkillLoadSource::Project;
            }
        }
        SkillLoadSource::Path
    };

    for raw_path in &options.skill_paths {
        let resolved = resolve_path(raw_path.to_string_lossy().trim(), &resolved_cwd);
        if !resolved.exists() {
            all_diagnostics.push(warning("skill path does not exist", &resolved));
            continue;
        }
        let source = get_source(&resolved);
        match std::fs::metadata(&resolved) {
            Ok(metadata) if metadata.is_dir() => {
                add_skills!(load_skills_from_dir(&resolved, source));
            }
            Ok(metadata) if metadata.is_file() && resolved.to_string_lossy().ends_with(".md") => {
                let (skill, diagnostics) = load_skill_from_file(&resolved, source);
                if let Some(skill) = skill {
                    all_diagnostics.extend(diagnostics);
                    add_skill(
                        skill,
                        &mut skills_by_name,
                        &mut name_order,
                        &mut real_paths,
                        &mut collision_diagnostics,
                    );
                } else {
                    all_diagnostics.extend(diagnostics);
                }
            }
            Ok(_) => {
                all_diagnostics.push(warning("skill path is not a markdown file", &resolved));
            }
            Err(error) => {
                all_diagnostics.push(warning(error.to_string(), &resolved));
            }
        }
    }

    LoadSkillsResult {
        skills: name_order
            .iter()
            .filter_map(|name| skills_by_name.get(name).cloned())
            .collect(),
        diagnostics: {
            all_diagnostics.extend(collision_diagnostics);
            all_diagnostics
        },
    }
}

// ---------------------------------------------------------------------------
// Prompt formatting + /skill expansion
// (skills.ts:327-370, system-prompt.ts:97-101,155-157, agent-session.ts:1301-1325)
// ---------------------------------------------------------------------------

/// `formatSkillsForPrompt` (skills.ts:335-361) with the read-tool gate of
/// system-prompt.ts inlined as a parameter: upstream only appends the
/// section when the `read` tool is active
/// (`tools.includes("read")`, or `selectedTools.includes("read")` for custom
/// prompts — pass `true` when no tool selection exists, which upstream
/// treats as "all tools").
///
/// Skills with `disable_model_invocation` are excluded from the prompt (they
/// can only be invoked explicitly via `/skill:name`). Returns an empty
/// string when nothing should be injected.
pub fn format_skills_for_prompt(skills: &[Skill], read_tool_active: bool) -> String {
    if !read_tool_active {
        return String::new();
    }
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description."
            .to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// `escapeXml` (skills.ts:363-370).
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `_expandSkillCommand` (agent-session.ts:1301-1325): expand
/// `/skill:name [args]` into the skill block. Returns the original text when
/// the input is not a skill command or the skill is unknown.
///
/// On file-read failure upstream emits an extension error and returns the
/// original text; here the failure is `Err(PirError::Resource)` and the
/// caller decides on the fallback (use the original text for parity).
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> Result<String, PirError> {
    let Some(rest) = text.strip_prefix("/skill:") else {
        return Ok(text.to_string());
    };
    let (skill_name, args) = match rest.find(' ') {
        Some(space_index) => (&rest[..space_index], rest[space_index + 1..].trim()),
        None => (rest, ""),
    };

    let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
        // Unknown skill, pass through.
        return Ok(text.to_string());
    };

    let content = std::fs::read_to_string(&skill.file_path).map_err(|error| {
        PirError::Resource(format!(
            "failed to read skill file {}: {error}",
            skill.file_path.display()
        ))
    })?;
    let body = strip_frontmatter(&content);
    let body = body.trim();
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{body}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display()
    );
    Ok(if args.is_empty() {
        skill_block
    } else {
        format!("{skill_block}\n\n{args}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers::TempDir;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("test path has parent"))
            .expect("create parent dirs");
        std::fs::write(path, content).expect("write fixture file");
    }

    fn make_skill(name: &str, description: &str, file_path: &Path) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            file_path: file_path.to_path_buf(),
            base_dir: file_path
                .parent()
                .expect("skill path has parent")
                .to_path_buf(),
            source_info: create_skill_source_info(
                file_path,
                file_path.parent().expect("skill path has parent"),
                SkillLoadSource::Path,
            ),
            disable_model_invocation: false,
        }
    }

    // ---- frontmatter ----

    #[test]
    fn test_parse_frontmatter_basic() {
        let (fm, body) = parse_frontmatter("---\nname: foo\ndescription: bar\n---\nbody text\n")
            .expect("valid yaml");
        assert_eq!(fm.name.as_deref(), Some("foo"));
        assert_eq!(fm.description.as_deref(), Some("bar"));
        assert!(!fm.disable_model_invocation);
        assert_eq!(body, "body text");
    }

    #[test]
    fn test_parse_frontmatter_crlf_and_disable_flag() {
        let (fm, body) = parse_frontmatter(
            "---\r\nname: foo\r\ndisable-model-invocation: true\r\n---\r\n\r\n  body\r\n",
        )
        .expect("valid yaml");
        assert_eq!(fm.name.as_deref(), Some("foo"));
        assert!(fm.disable_model_invocation);
        assert_eq!(body, "body");
    }

    #[test]
    fn test_extract_frontmatter_no_frontmatter_keeps_body_untrimmed() {
        let (yaml, body) = extract_frontmatter("\n\nhello\n");
        assert!(yaml.is_none());
        assert_eq!(body, "\n\nhello\n");
        let (yaml, body) = extract_frontmatter("---\nunterminated");
        assert!(yaml.is_none());
        assert_eq!(body, "---\nunterminated");
    }

    #[test]
    fn test_parse_frontmatter_empty_and_non_mapping_yaml() {
        // `---\n---\nbody`: JS slice(4, 3) is an empty string → parse → null → {}.
        let (fm, body) = parse_frontmatter("---\n---\nbody").expect("empty yaml parses");
        assert!(fm.name.is_none());
        assert_eq!(body, "body");

        // A scalar YAML document reads all fields as absent (upstream's
        // unchecked cast).
        let (fm, _) = parse_frontmatter("---\njust a string\n---\nbody").expect("scalar parses");
        assert!(fm.name.is_none());
        assert!(fm.description.is_none());
    }

    #[test]
    fn test_parse_frontmatter_invalid_yaml_errors() {
        assert!(parse_frontmatter("---\n: [unclosed\n---\nbody").is_err());
    }

    // ---- validation ----

    #[test]
    fn test_validate_name_rules() {
        assert!(validate_name("valid-name-1").is_empty());
        assert_eq!(
            validate_name(&"a".repeat(65)),
            vec![format!("name exceeds {MAX_NAME_LENGTH} characters (65)")]
        );
        assert!(validate_name("Invalid_Name")
            .iter()
            .any(|e| e.contains("invalid characters")));
        assert!(validate_name("-bad")
            .iter()
            .any(|e| e.contains("start or end with a hyphen")));
        assert!(validate_name("bad-")
            .iter()
            .any(|e| e.contains("start or end with a hyphen")));
        assert!(validate_name("bad--name")
            .iter()
            .any(|e| e.contains("consecutive hyphens")));
    }

    #[test]
    fn test_validate_description_rules() {
        assert_eq!(validate_description(None), vec!["description is required"]);
        assert_eq!(
            validate_description(Some("   ")),
            vec!["description is required"]
        );
        let long = "a".repeat(1025);
        assert_eq!(
            validate_description(Some(&long)),
            vec![format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters (1025)"
            )]
        );
        assert!(validate_description(Some("ok")).is_empty());
    }

    // ---- glob matcher ----

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.md", "foo.md"));
        assert!(!glob_match("*.md", "sub/foo.md"));
        assert!(glob_match("**/*.md", "sub/foo.md"));
        assert!(glob_match("**/*.md", "a/b/c.md"));
        assert!(glob_match("a/**/b", "a/b"));
        assert!(glob_match("a/**/b", "a/x/y/b"));
        assert!(glob_match("foo?", "foob"));
        assert!(!glob_match("foo?", "foo/b"));
        assert!(glob_match("skills/**", "skills/a/b/c"));
        assert!(!glob_match("*.md", "foo.txt"));
    }

    // ---- pattern filtering ----

    #[test]
    fn test_apply_patterns_include_exclude_force() {
        let base = Path::new("/base");
        let files: Vec<PathBuf> = ["skills/a.md", "skills/b.md", "skills/skip.md"]
            .iter()
            .map(|p| base.join(p))
            .collect();
        // Include only skills/*.md, then exclude skip.md.
        let enabled = apply_patterns(
            &files,
            &["skills/*.md".to_string(), "!*skip*".to_string()],
            base,
        );
        assert!(enabled.contains(&base.join("skills/a.md")));
        assert!(!enabled.contains(&base.join("skills/skip.md")));

        // `-` force-exclude wins over `+` force-include (exact paths).
        let enabled = apply_patterns(
            &files,
            &["!*skip*".to_string(), "+skills/skip.md".to_string()],
            base,
        );
        assert!(enabled.contains(&base.join("skills/skip.md")));
        let enabled = apply_patterns(
            &files,
            &[
                "!*skip*".to_string(),
                "+skills/skip.md".to_string(),
                "-skills/skip.md".to_string(),
            ],
            base,
        );
        assert!(!enabled.contains(&base.join("skills/skip.md")));
    }

    #[test]
    fn test_apply_patterns_skill_parent_dir_matching() {
        // SKILL.md files also match against their parent dir (name/rel/abs).
        let base = Path::new("/base");
        let files = vec![base.join("skills/myskill/SKILL.md")];
        let enabled = apply_patterns(&files, &["!myskill".to_string()], base);
        assert!(enabled.is_empty());
    }

    #[test]
    fn test_is_enabled_by_overrides() {
        let base = Path::new("/base");
        let file = base.join("skills/foo.md");
        assert!(is_enabled_by_overrides(&file, &[], base));
        assert!(!is_enabled_by_overrides(
            &file,
            &["!foo*".to_string()],
            base
        ));
        assert!(is_enabled_by_overrides(
            &file,
            &["!foo*".to_string(), "+skills/foo.md".to_string()],
            base
        ));
        assert!(!is_enabled_by_overrides(
            &file,
            &[
                "!foo*".to_string(),
                "+skills/foo.md".to_string(),
                "-skills/foo.md".to_string()
            ],
            base
        ));
    }

    // ---- directory walking ----

    #[test]
    fn test_collect_skill_entries_pir_mode_loose_md_and_skill_roots() {
        let tmp = TempDir::new();
        let root = tmp.path().join("skills");
        write(&root.join("loose.md"), "---\ndescription: x\n---\n");
        write(&root.join("notes.txt"), "not a skill");
        write(&root.join("mydir/SKILL.md"), "---\ndescription: x\n---\n");
        // Nested SKILL.md beneath a skill root is not reached.
        write(
            &root.join("mydir/deep/SKILL.md"),
            "---\ndescription: x\n---\n",
        );

        let entries = collect_skill_entries(&root, SkillDiscoveryMode::Pir);
        assert!(entries.contains(&root.join("mydir/SKILL.md")));
        assert!(entries.contains(&root.join("loose.md")));
        assert!(!entries.contains(&root.join("notes.txt")));
        assert!(!entries.contains(&root.join("mydir/deep/SKILL.md")));
    }

    #[test]
    fn test_collect_skill_entries_agents_mode_ignores_loose_md() {
        let tmp = TempDir::new();
        let root = tmp.path().join("skills");
        write(&root.join("loose.md"), "---\ndescription: x\n---\n");
        write(&root.join("mydir/SKILL.md"), "---\ndescription: x\n---\n");

        let entries = collect_skill_entries(&root, SkillDiscoveryMode::Agents);
        assert_eq!(entries, vec![root.join("mydir/SKILL.md")]);
    }

    #[test]
    fn test_collect_skill_entries_root_skill_md_stops_everything() {
        let tmp = TempDir::new();
        let root = tmp.path().join("skills");
        write(&root.join("SKILL.md"), "---\ndescription: x\n---\n");
        write(&root.join("loose.md"), "---\ndescription: x\n---\n");
        write(&root.join("sub/SKILL.md"), "---\ndescription: x\n---\n");

        let entries = collect_skill_entries(&root, SkillDiscoveryMode::Pir);
        assert_eq!(entries, vec![root.join("SKILL.md")]);
    }

    #[test]
    fn test_collect_skill_entries_skips_dotdirs_node_modules_and_ignored() {
        let tmp = TempDir::new();
        let root = tmp.path().join("skills");
        write(&root.join(".hidden/SKILL.md"), "---\ndescription: x\n---\n");
        write(
            &root.join("node_modules/SKILL.md"),
            "---\ndescription: x\n---\n",
        );
        write(&root.join("ignored.md"), "---\ndescription: x\n---\n");
        write(&root.join(".gitignore"), "ignored.md\n");
        write(&root.join("kept.md"), "---\ndescription: x\n---\n");
        // An ignored SKILL.md does not make its dir a skill root; recursion
        // continues below it (upstream first-pass `continue`). The pattern
        // is anchored (`/SKILL.md`) so the deeper SKILL.md still matches —
        // an unanchored `SKILL.md` pattern would ignore at any depth here
        // (gitignore semantics; see module docs).
        write(&root.join("sub/.gitignore"), "/SKILL.md\n");
        write(&root.join("sub/SKILL.md"), "---\ndescription: x\n---\n");
        write(
            &root.join("sub/deep/SKILL.md"),
            "---\ndescription: x\n---\n",
        );

        let entries = collect_skill_entries(&root, SkillDiscoveryMode::Pir);
        assert!(entries.contains(&root.join("kept.md")));
        assert!(entries.contains(&root.join("sub/deep/SKILL.md")));
        assert!(!entries.contains(&root.join("ignored.md")));
        assert!(!entries.contains(&root.join(".hidden/SKILL.md")));
        assert!(!entries.contains(&root.join("node_modules/SKILL.md")));
        assert!(!entries.contains(&root.join("sub/SKILL.md")));
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_skill_entries_follows_symlinks() {
        let tmp = TempDir::new();
        let real = tmp.path().join("real");
        write(&real.join("SKILL.md"), "---\ndescription: x\n---\n");
        let root = tmp.path().join("skills");
        std::fs::create_dir_all(&root).expect("create root");
        std::os::unix::fs::symlink(&real, root.join("linked")).expect("symlink dir");
        std::os::unix::fs::symlink(root.join("missing"), root.join("broken")).expect("broken link");

        let entries = collect_skill_entries(&root, SkillDiscoveryMode::Agents);
        assert_eq!(entries, vec![root.join("linked/SKILL.md")]);
    }

    // ---- ancestor scan ----

    #[test]
    fn test_ancestor_scan_stops_at_git_repo_root_inclusive() {
        let tmp = TempDir::new();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("create .git");
        let cwd = repo.join("sub/dir");
        std::fs::create_dir_all(&cwd).expect("create cwd");

        let dirs = collect_ancestor_agents_skill_dirs(&cwd);
        assert_eq!(
            dirs,
            vec![
                cwd.join(".agents/skills"),
                repo.join("sub/.agents/skills"),
                repo.join(".agents/skills"),
            ]
        );
    }

    #[test]
    fn test_ancestor_scan_without_git_repo_reaches_filesystem_root() {
        let tmp = TempDir::new();
        let cwd = tmp.path().join("a/b");
        std::fs::create_dir_all(&cwd).expect("create cwd");

        let dirs = collect_ancestor_agents_skill_dirs(&cwd);
        assert_eq!(dirs.first(), Some(&cwd.join(".agents/skills")));
        assert_eq!(dirs.last(), Some(&PathBuf::from("/.agents/skills")));
        assert!(dirs.contains(&tmp.path().join(".agents/skills")));
    }

    #[test]
    fn test_find_git_repo_root_accepts_git_file() {
        let tmp = TempDir::new();
        // A worktree/submodule has `.git` as a file, not a directory.
        write(&tmp.path().join("repo/.git"), "gitdir: /elsewhere\n");
        let cwd = tmp.path().join("repo/sub");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        assert_eq!(find_git_repo_root(&cwd), Some(tmp.path().join("repo")));
        assert_eq!(find_git_repo_root(tmp.path()), None);
    }

    // ---- loading ----

    #[test]
    fn test_load_skill_from_file_description_missing_drops_skill() {
        let tmp = TempDir::new();
        let file = tmp.path().join("myskill/SKILL.md");
        write(&file, "---\nname: myskill\n---\nbody");

        let (skill, diagnostics) = load_skill_from_file(&file, SkillLoadSource::Path);
        assert!(skill.is_none());
        assert!(diagnostics
            .iter()
            .any(|d| d.message == "description is required"));
    }

    #[test]
    fn test_load_skill_from_file_warns_but_loads_and_falls_back_to_dir_name() {
        let tmp = TempDir::new();
        let file = tmp.path().join("ParentDir/SKILL.md");
        write(&file, "---\ndescription: has one\n---\nbody");

        let (skill, diagnostics) = load_skill_from_file(&file, SkillLoadSource::Project);
        let skill = skill.expect("skill loads despite name warnings");
        assert_eq!(skill.name, "ParentDir");
        assert_eq!(skill.description, "has one");
        assert!(!skill.disable_model_invocation);
        assert_eq!(skill.source_info.scope, SourceScope::Project);
        assert_eq!(skill.source_info.source, "local");
        assert!(diagnostics
            .iter()
            .any(|d| d.message.contains("invalid characters")));
    }

    #[test]
    fn test_load_skills_name_collision_first_wins_with_diagnostic() {
        let tmp = TempDir::new();
        let first = tmp.path().join("a/SKILL.md");
        let second = tmp.path().join("b/SKILL.md");
        write(&first, "---\nname: dup\ndescription: first\n---\n");
        write(&second, "---\nname: dup\ndescription: second\n---\n");

        let result = load_skills(&LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![first.clone(), second.clone()],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].description, "first");
        let collision = result
            .diagnostics
            .iter()
            .find(|d| d.kind == DiagnosticKind::Collision)
            .expect("collision diagnostic");
        assert_eq!(collision.message, "name \"dup\" collision");
        let detail = collision.collision.as_ref().expect("collision detail");
        assert_eq!(detail.resource_type, DiagnosticResourceType::Skill);
        assert_eq!(detail.winner_path, first);
        assert_eq!(detail.loser_path, second);
    }

    #[cfg(unix)]
    #[test]
    fn test_load_skills_realpath_dedupe_skips_symlinked_duplicates() {
        let tmp = TempDir::new();
        let real = tmp.path().join("real/SKILL.md");
        write(&real, "---\nname: s\ndescription: d\n---\n");
        let link = tmp.path().join("link.md");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let result = load_skills(&LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![real, link],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 1);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Collision));
    }

    #[test]
    fn test_load_skills_path_warnings_and_source_classification() {
        let tmp = TempDir::new();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        let user_skill = agent_dir.join("skills/u/SKILL.md");
        let project_skill = cwd.join(".pir/skills/p/SKILL.md");
        let other_skill = tmp.path().join("elsewhere/o/SKILL.md");
        write(&user_skill, "---\nname: u\ndescription: d\n---\n");
        write(&project_skill, "---\nname: p\ndescription: d\n---\n");
        write(&other_skill, "---\nname: o\ndescription: d\n---\n");
        let not_md = tmp.path().join("elsewhere/note.txt");
        write(&not_md, "text");

        let result = load_skills(&LoadSkillsOptions {
            cwd: cwd.clone(),
            agent_dir,
            skill_paths: vec![
                user_skill,
                project_skill,
                other_skill,
                not_md,
                tmp.path().join("missing"),
            ],
            include_defaults: false,
        });
        let scope_of = |name: &str| {
            result
                .skills
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.source_info.scope)
        };
        assert_eq!(scope_of("u"), Some(SourceScope::User));
        assert_eq!(scope_of("p"), Some(SourceScope::Project));
        assert_eq!(scope_of("o"), Some(SourceScope::Temporary));
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.message == "skill path is not a markdown file"));
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.message == "skill path does not exist"));
    }

    // ---- discovery ----

    fn discovery_options(tmp: &TempDir) -> DiscoverSkillsOptions {
        DiscoverSkillsOptions {
            cwd: tmp.path().join("project"),
            agent_dir: tmp.path().join("agent"),
            home_dir: Some(tmp.path().join("home")),
            project_trusted: true,
            global_settings_skills: Vec::new(),
            project_settings_skills: Vec::new(),
            cli_skill_paths: Vec::new(),
        }
    }

    #[test]
    fn test_discover_skill_paths_rank_order_and_trust_gate() {
        let tmp = TempDir::new();
        let options = discovery_options(&tmp);
        write(
            &options.cwd.join(".pir/skills/proj-auto/SKILL.md"),
            "---\ndescription: x\n---\n",
        );
        write(
            &options.agent_dir.join("skills/user-auto/SKILL.md"),
            "---\ndescription: x\n---\n",
        );
        write(
            &options
                .home_dir
                .as_ref()
                .expect("home")
                .join(".agents/skills/user-agents/SKILL.md"),
            "---\ndescription: x\n---\n",
        );
        let project_settings_dir = options.cwd.join(".pir/declared");
        write(
            &project_settings_dir.join("declared/SKILL.md"),
            "---\ndescription: x\n---\n",
        );

        let mut options = options;
        options.project_settings_skills = vec!["./declared".to_string()];

        let paths = discover_skill_paths(&options);
        let ranks: Vec<u8> = paths
            .iter()
            .map(|p| resource_precedence_rank(&p.metadata))
            .collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(ranks, sorted, "rank-sorted output");
        assert_eq!(
            paths[0].path,
            project_settings_dir.join("declared/SKILL.md")
        );
        assert_eq!(paths[0].metadata.scope, SourceScope::Project);
        assert!(paths.iter().all(|p| p.enabled));

        // Trust gate off: project auto-discovered paths disappear, the
        // project settings entry remains (upstream never gates settings).
        options.project_trusted = false;
        let paths = discover_skill_paths(&options);
        assert!(!paths
            .iter()
            .any(|p| p.path.starts_with(options.cwd.join(".pir/skills"))));
        assert!(paths
            .iter()
            .any(|p| p.path.starts_with(&project_settings_dir)));
        assert!(paths
            .iter()
            .any(|p| p.path.starts_with(options.agent_dir.join("skills"))));
    }

    #[test]
    fn test_discover_skill_paths_ancestors_exclude_home_and_dedupe_canonical() {
        let tmp = TempDir::new();
        // home inside the project tree: its `.agents/skills` must not appear
        // twice (once via ancestors, once as the user location).
        let cwd = tmp.path().join("home/project");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let home_skill = tmp.path().join("home/.agents/skills/hs/SKILL.md");
        write(&home_skill, "---\ndescription: x\n---\n");

        let options = DiscoverSkillsOptions {
            cwd: cwd.clone(),
            home_dir: Some(tmp.path().join("home")),
            ..discovery_options(&tmp)
        };
        let paths = discover_skill_paths(&options);
        let matches: Vec<_> = paths.iter().filter(|p| p.path == home_skill).collect();
        assert_eq!(matches.len(), 1, "home .agents skill appears exactly once");
        assert_eq!(matches[0].metadata.scope, SourceScope::User);
    }

    #[test]
    fn test_discover_skill_paths_overrides_and_cli_last() {
        let tmp = TempDir::new();
        let options = discovery_options(&tmp);
        let excluded = options.agent_dir.join("skills/excluded/SKILL.md");
        write(&excluded, "---\ndescription: x\n---\n");
        write(
            &options.agent_dir.join("skills/kept/SKILL.md"),
            "---\ndescription: x\n---\n",
        );
        let cli_skill = tmp.path().join("cli/SKILL.md");
        write(&cli_skill, "---\ndescription: x\n---\n");

        let mut options = options;
        options.global_settings_skills = vec!["!excluded".to_string()];
        options.cli_skill_paths = vec![cli_skill.to_string_lossy().into_owned()];

        let paths = discover_skill_paths(&options);
        let excluded_entry = paths
            .iter()
            .find(|p| p.path == excluded)
            .expect("excluded entry still listed");
        assert!(!excluded_entry.enabled);
        assert_eq!(
            paths.last().map(|p| &p.path),
            Some(&cli_skill),
            "CLI paths come last"
        );
    }

    // ---- prompt formatting ----

    #[test]
    fn test_format_skills_for_prompt_exact_output_and_escaping() {
        let tmp = TempDir::new();
        let file = tmp.path().join("s/SKILL.md");
        let mut skill = make_skill("a&b", "desc <with> \"quotes\" & 'apos'", &file);

        let want = vec![
            "",
            "",
            "The following skills provide specialized instructions for specific tasks.",
            "Use the read tool to load a skill's file when the task matches its description.",
            "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
            "",
            "<available_skills>",
            "  <skill>",
            "    <name>a&amp;b</name>",
            "    <description>desc &lt;with&gt; &quot;quotes&quot; &amp; &apos;apos&apos;</description>",
            &format!("    <location>{}</location>", file.display()),
            "  </skill>",
            "</available_skills>",
        ]
        .join("\n");
        assert_eq!(
            format_skills_for_prompt(std::slice::from_ref(&skill), true),
            want
        );

        // disable-model-invocation skills are hidden from the prompt.
        skill.disable_model_invocation = true;
        assert_eq!(format_skills_for_prompt(&[skill], true), "");
    }

    #[test]
    fn test_format_skills_for_prompt_read_tool_gate() {
        let tmp = TempDir::new();
        let file = tmp.path().join("s/SKILL.md");
        let skill = make_skill("s", "d", &file);
        assert_eq!(
            format_skills_for_prompt(std::slice::from_ref(&skill), false),
            ""
        );
        assert!(!format_skills_for_prompt(&[skill], true).is_empty());
        assert_eq!(format_skills_for_prompt(&[], true), "");
    }

    // ---- /skill expansion ----

    #[test]
    fn test_expand_skill_command_exact_block_and_args() {
        let tmp = TempDir::new();
        let file = tmp.path().join("myskill/SKILL.md");
        write(
            &file,
            "---\nname: myskill\ndescription: d\n---\n\nDo the thing.\n",
        );
        let skill = make_skill("myskill", "d", &file);

        let got =
            expand_skill_command("/skill:myskill", std::slice::from_ref(&skill)).expect("expand");
        let want = format!(
            "<skill name=\"myskill\" location=\"{}\">\nReferences are relative to {}.\n\nDo the thing.\n</skill>",
            file.display(),
            tmp.path().join("myskill").display()
        );
        assert_eq!(got, want);

        let got = expand_skill_command("/skill:myskill   do it now  ", &[skill]).expect("expand");
        assert_eq!(got, format!("{want}\n\ndo it now"));
    }

    #[test]
    fn test_expand_skill_command_passthrough_cases() {
        let tmp = TempDir::new();
        let file = tmp.path().join("s/SKILL.md");
        let skill = make_skill("s", "d", &file);

        assert_eq!(
            expand_skill_command("hello", std::slice::from_ref(&skill)).expect("ok"),
            "hello"
        );
        assert_eq!(
            expand_skill_command("/skill:unknown arg", std::slice::from_ref(&skill)).expect("ok"),
            "/skill:unknown arg"
        );
        // Missing file → Err (callers fall back to the original text).
        assert!(expand_skill_command("/skill:s", std::slice::from_ref(&skill)).is_err());
    }
}
