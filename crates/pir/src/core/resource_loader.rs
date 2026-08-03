//! Port of `packages/coding-agent/src/core/resource-loader.ts` @ pi 0.82.1
//! (2efa728): `DefaultResourceLoader` (:162-1043) — the unified resource
//! discovery pipeline that wires settings, skills, prompt templates, themes
//! and context files into one loaded set — plus the prompts/themes discovery
//! subset of `core/package-manager.ts` (`resolve` :901-953,
//! `resolveLocalEntries` :2280-2301, `addAutoDiscoveredResources` :2303-2467,
//! `collectFiles`/`collectAuto*Entries` :301-346, :462-530, `toResolvedPaths`
//! :2527-2545) and the keybindings config-file migration of `migrations.ts`
//! (`migrateKeybindingsConfigFile` :157-172).
//!
//! Discovery order (coding-standards §10.2): global `~/.pir/agent` → project
//! `.pir` (trust-gated) → settings-specified paths → CLI flags → packages.
//! Precedence rank (package-manager.ts:184-188): project settings (0) >
//! project auto (1) > user settings (2) > user auto (3) > package (4); lower
//! rank wins name collisions (first-wins + `collision` diagnostics).
//!
//! Intentional differences:
//! - Extensions are not loaded (the extension host lands in T15);
//!   [`LoadedExtensions`] is a discovery placeholder: `additional_extension_paths`
//!   are resolved and existence-checked (resource-loader.ts:408-415) but
//!   nothing is executed, and settings/auto/package extension discovery is
//!   deferred to the package manager (T14). `no_extensions` is accepted for
//!   interface parity but has no observable effect in this slice.
//! - Package resources arrive through the [`PackageResourcePaths`] input port
//!   (T14 installs/resolves them); they merge at precedence rank 4. For
//!   skills they are appended after `skills::discover_skill_paths` output
//!   (that pipeline is owned by `skills.rs`), so a path that is both
//!   auto-discovered and package-provided keeps the auto metadata instead of
//!   the upstream package metadata — observable only through the enabled
//!   flag / source info of that overlapping path.
//! - The SDK override hooks (`extensionsOverride` … `appendSystemPromptOverride`,
//!   resource-loader.ts:142-159) and inline extension factories are not
//!   ported.
//! - `PromptTemplate` and `Theme` carry no `sourceInfo` in this port (see
//!   prompt_templates.rs / themes.rs), so the metadata-based source-info
//!   refinement (`findSourceInfoForPath`, resource-loader.ts:701-745) applies
//!   to skills only.
//! - Warnings upstream prints to stderr (`console.error` + chalk) become
//!   structured [`ResourceDiagnostic`]s or `tracing::warn!`.
//! - The keybindings migration write-back keeps upstream's silent handling of
//!   malformed content but propagates lock/write I/O failures as
//!   `PirError::Resource` (upstream swallows them too).
//! - The themes "path does not exist" CLI check is intentionally *not* gated
//!   on `isLocalPath`, mirroring the upstream inconsistency
//!   (resource-loader.ts:459-464 vs. the gated skills/prompts loops
//!   :425-451).
//! - ADR-0001 renames apply: `.pi` → `.pir`, `PI_` → `PIR_`.
//!
//! Sync `std::fs` I/O mirrors the upstream sync methods (see
//! session_manager.rs for the established pattern); async callers must wrap
//! calls in `tokio::task::spawn_blocking`.

use std::collections::HashSet;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use ignore::WalkBuilder;
use serde_json::Value;

use crate::config;
use crate::core::keybindings::migrate_keybindings_config;
use crate::core::prompt_templates::{
    load_prompt_templates, LoadPromptTemplatesOptions, PromptTemplate,
};
use crate::core::settings_manager::{Settings, SettingsManager};
use crate::core::skills::{
    apply_patterns, canonicalize_path, discover_skill_paths, is_enabled_by_overrides, load_skills,
    resource_precedence_rank, DiscoverSkillsOptions, LoadSkillsOptions, LoadSkillsResult,
    MetadataSource, PathMetadata, Skill, SourceInfo, SourceOrigin, SourceScope,
};
use crate::core::system_prompt::{
    discover_append_system_prompt_file, discover_system_prompt_file, load_project_context_files,
    resolve_prompt_input, ContextFile,
};
use crate::core::themes::{load_theme_from_path, Theme};
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

// Diagnostics types are ported in `skills.rs` (diagnostics.ts); re-exported
// here exactly like upstream (`export type { ResourceCollision,
// ResourceDiagnostic } from "./diagnostics.ts"`, resource-loader.ts:8).
pub use crate::core::skills::{
    DiagnosticKind, DiagnosticResourceType, ResourceCollision, ResourceDiagnostic,
};

/// fs2 lock retry budget, mirroring `settings_manager.rs`
/// (`acquireLockSyncWithRetry`, settings-manager.ts:199-224).
const LOCK_MAX_ATTEMPTS: u32 = 10;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Loaded output (design §6.7)
// ---------------------------------------------------------------------------

/// One extension path error (`LoadExtensionsResult["errors"][number]`,
/// extensions/types.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionLoadError {
    pub path: PathBuf,
    pub error: String,
}

/// Placeholder for `LoadExtensionsResult` (extensions/loader.ts). Extension
/// *loading* lands with the extension host (T15); this slice only resolves
/// and existence-checks the CLI-provided extension paths.
#[derive(Debug, Clone, Default)]
pub struct LoadedExtensions {
    /// Resolved CLI extension paths that exist on disk.
    pub paths: Vec<PathBuf>,
    /// Existence-check failures (resource-loader.ts:408-415).
    pub errors: Vec<ExtensionLoadError>,
}

/// The aggregate output of the discovery pipeline (design §6.7
/// `LoadedResources`). `system_prompt` / `append_system_prompt` ride along
/// because upstream's loader resolves them in the same `reload()` pass
/// (resource-loader.ts:477-491).
#[derive(Debug, Default)]
pub struct LoadedResources {
    pub extensions: LoadedExtensions,
    pub skills: Vec<Skill>,
    pub prompts: Vec<PromptTemplate>,
    pub themes: Vec<Theme>,
    pub context_files: Vec<ContextFile>,
    /// Skills + prompts + themes diagnostics, in pipeline order.
    pub diagnostics: Vec<ResourceDiagnostic>,
    /// `--system-prompt` / discovered `SYSTEM.md`, resolved (file vs inline).
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` / discovered `APPEND_SYSTEM.md`, resolved.
    pub append_system_prompt: Vec<String>,
}

// ---------------------------------------------------------------------------
// Input ports
// ---------------------------------------------------------------------------

/// One pre-resolved package resource (T14 input port). Upstream these arrive
/// as `ResolvedResource` entries with `origin: "package"` metadata, which
/// pins them at precedence rank 4 (package-manager.ts:184-188).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageResource {
    pub path: PathBuf,
    pub enabled: bool,
    /// Scope of the installing package (used for skill `SourceInfo`).
    pub scope: SourceScope,
    /// Base directory of the resource inside the package, when known.
    pub base_dir: Option<PathBuf>,
}

/// Pre-resolved resources from installed packages (`settings.packages`),
/// merged into discovery at rank 4. Empty until T14 wires the package
/// manager.
#[derive(Debug, Clone, Default)]
pub struct PackageResourcePaths {
    pub extension_paths: Vec<PackageResource>,
    pub skill_paths: Vec<PackageResource>,
    pub prompt_paths: Vec<PackageResource>,
    pub theme_paths: Vec<PackageResource>,
}

/// One path contributed through the `resources_discover` extension event
/// (`ResourceExtensionPaths` entries, resource-loader.ts:28-32). The caller
/// (extension host, T15) supplies the `SourceInfo` — e.g.
/// `source: "extension:<name>"` — which overrides the loaded resources'
/// provenance for everything under `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceExtensionPath {
    pub path: PathBuf,
    pub source_info: SourceInfo,
}

/// `ResourceExtensionPaths` (resource-loader.ts:28-32) — the
/// `resources_discover` hook payload.
#[derive(Debug, Clone, Default)]
pub struct ResourceExtensionPaths {
    pub skill_paths: Vec<ResourceExtensionPath>,
    pub prompt_paths: Vec<ResourceExtensionPath>,
    pub theme_paths: Vec<ResourceExtensionPath>,
}

/// `DefaultResourceLoaderOptions` (resource-loader.ts:125-160), minus the
/// SDK override hooks and inline extension factories (see header).
pub struct DefaultResourceLoaderOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// User home directory for `~/.agents/skills` discovery; read from the
    /// environment when `None` (mirrors `DiscoverSkillsOptions::home_dir` —
    /// explicit so tests can isolate from the real `$HOME`).
    pub home_dir: Option<PathBuf>,
    /// Caller-owned settings manager; created from `cwd`/`agent_dir` when
    /// absent (resource-loader.ts:220).
    pub settings_manager: Option<SettingsManager>,
    /// `-e` extension paths (temporary scope).
    pub additional_extension_paths: Vec<String>,
    /// `--skill` paths.
    pub additional_skill_paths: Vec<String>,
    /// `--prompt-template` paths.
    pub additional_prompt_template_paths: Vec<String>,
    /// `--theme` paths.
    pub additional_theme_paths: Vec<String>,
    /// Pre-resolved package resources (T14 input port).
    pub package_resources: PackageResourcePaths,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
    /// `--system-prompt` value (file path or inline text).
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` values (file paths or inline text).
    pub append_system_prompt: Option<Vec<String>>,
}

impl DefaultResourceLoaderOptions {
    pub fn new(cwd: PathBuf, agent_dir: PathBuf) -> Self {
        Self {
            cwd,
            agent_dir,
            settings_manager: None,
            home_dir: None,
            additional_extension_paths: Vec::new(),
            additional_skill_paths: Vec::new(),
            additional_prompt_template_paths: Vec::new(),
            additional_theme_paths: Vec::new(),
            package_resources: PackageResourcePaths::default(),
            no_extensions: false,
            no_skills: false,
            no_prompt_templates: false,
            no_themes: false,
            no_context_files: false,
            system_prompt: None,
            append_system_prompt: None,
        }
    }
}

// ---------------------------------------------------------------------------
// DefaultResourceLoader
// ---------------------------------------------------------------------------

/// `DefaultResourceLoader` (resource-loader.ts:162-1043). Stateful because
/// `extendResources` merges new paths into the last-loaded path sets
/// (resource-loader.ts:209-214, 293-331) and the two-phase trust flow (T14)
/// reloads after flipping `project_trusted`.
pub struct DefaultResourceLoader {
    cwd: PathBuf,
    agent_dir: PathBuf,
    home_dir: Option<PathBuf>,
    settings_manager: SettingsManager,
    additional_extension_paths: Vec<String>,
    additional_skill_paths: Vec<String>,
    additional_prompt_template_paths: Vec<String>,
    additional_theme_paths: Vec<String>,
    package_resources: PackageResourcePaths,
    no_extensions: bool,
    no_skills: bool,
    no_prompt_templates: bool,
    no_themes: bool,
    no_context_files: bool,
    system_prompt_source: Option<String>,
    append_system_prompt_source: Option<Vec<String>>,

    resources: LoadedResources,
    skill_diagnostics: Vec<ResourceDiagnostic>,
    prompt_diagnostics: Vec<ResourceDiagnostic>,
    theme_diagnostics: Vec<ResourceDiagnostic>,

    /// `resources_discover` contributions (resource-loader.ts:210-212).
    extension_skill_source_infos: Vec<(PathBuf, SourceInfo)>,
    extension_prompt_source_infos: Vec<(PathBuf, SourceInfo)>,
    extension_theme_source_infos: Vec<(PathBuf, SourceInfo)>,
    /// Last loaded path sets, merged into by `extend_resources`
    /// (resource-loader.ts:209, 213-214).
    last_skill_paths: Vec<PathBuf>,
    last_prompt_paths: Vec<PathBuf>,
    last_theme_paths: Vec<PathBuf>,
    loaded: bool,
}

impl DefaultResourceLoader {
    pub fn new(options: DefaultResourceLoaderOptions) -> Self {
        let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let cwd = resolve_path(&options.cwd.to_string_lossy(), &process_cwd);
        let agent_dir = resolve_path(&options.agent_dir.to_string_lossy(), &process_cwd);
        let settings_manager = options
            .settings_manager
            .unwrap_or_else(|| SettingsManager::create(&cwd, Some(&agent_dir), Default::default()));
        Self {
            cwd,
            agent_dir,
            home_dir: options.home_dir,
            settings_manager,
            additional_extension_paths: options.additional_extension_paths,
            additional_skill_paths: options.additional_skill_paths,
            additional_prompt_template_paths: options.additional_prompt_template_paths,
            additional_theme_paths: options.additional_theme_paths,
            package_resources: options.package_resources,
            no_extensions: options.no_extensions,
            no_skills: options.no_skills,
            no_prompt_templates: options.no_prompt_templates,
            no_themes: options.no_themes,
            no_context_files: options.no_context_files,
            system_prompt_source: options.system_prompt,
            append_system_prompt_source: options.append_system_prompt,
            resources: LoadedResources::default(),
            skill_diagnostics: Vec::new(),
            prompt_diagnostics: Vec::new(),
            theme_diagnostics: Vec::new(),
            extension_skill_source_infos: Vec::new(),
            extension_prompt_source_infos: Vec::new(),
            extension_theme_source_infos: Vec::new(),
            last_skill_paths: Vec::new(),
            last_prompt_paths: Vec::new(),
            last_theme_paths: Vec::new(),
            loaded: false,
        }
    }

    pub fn settings_manager(&self) -> &SettingsManager {
        &self.settings_manager
    }

    pub fn settings_manager_mut(&mut self) -> &mut SettingsManager {
        &mut self.settings_manager
    }

    pub fn resources(&self) -> &LoadedResources {
        &self.resources
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Diagnostics of the skills pipeline (`getSkills().diagnostics`).
    pub fn skill_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.skill_diagnostics
    }

    /// Diagnostics of the prompt-templates pipeline (`getPrompts().diagnostics`).
    pub fn prompt_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.prompt_diagnostics
    }

    /// Diagnostics of the themes pipeline (`getThemes().diagnostics`).
    pub fn theme_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.theme_diagnostics
    }

    pub fn is_project_trusted(&self) -> bool {
        self.settings_manager.is_project_trusted()
    }

    /// `resources_discover` provenance contributed for prompt paths
    /// (resource-loader.ts:211). Prompt templates carry no `sourceInfo` in
    /// this port, so the extension host (T15) consumes this directly.
    pub fn extension_prompt_source_infos(&self) -> &[(PathBuf, SourceInfo)] {
        &self.extension_prompt_source_infos
    }

    /// `resources_discover` provenance contributed for theme paths
    /// (resource-loader.ts:212) — same consumer as above.
    pub fn extension_theme_source_infos(&self) -> &[(PathBuf, SourceInfo)] {
        &self.extension_theme_source_infos
    }

    /// Two-phase trust grouping (requirements §7.8): the pre-trust pass runs
    /// [`reload`](Self::reload) with `project_trusted == false` — only
    /// global/user + CLI resources and context files load; T14 resolves
    /// trust, calls this, and reloads for the post-trust group
    /// (resource-loader.ts:333-353, trust resolution callback deferred).
    pub fn set_project_trusted(&mut self, trusted: bool) {
        self.settings_manager.set_project_trusted(trusted);
    }

    /// `reload()` (resource-loader.ts:341-493), minus extension/package
    /// loading. All failures degrade to diagnostics — this never errors.
    pub fn reload(&mut self) {
        self.settings_manager.reload();
        let trusted = self.settings_manager.is_project_trusted();

        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();
        let global_skill_entries = settings_string_array(&global_settings, "skills");
        let project_skill_entries = settings_string_array(&project_settings, "skills");
        let global_prompt_entries = settings_string_array(&global_settings, "prompts");
        let project_prompt_entries = settings_string_array(&project_settings, "prompts");
        let global_theme_entries = settings_string_array(&global_settings, "themes");
        let project_theme_entries = settings_string_array(&project_settings, "themes");

        self.reload_extensions();

        // --- Skills (resource-loader.ts:419-432) ---
        let (skill_paths, skill_source_map) =
            self.compute_skill_paths(trusted, &global_skill_entries, &project_skill_entries);
        self.last_skill_paths = skill_paths.clone();
        self.update_skills_from_paths(&skill_paths, &skill_source_map);
        // resource-loader.ts:425-432: missing CLI skill paths (isLocalPath-gated).
        for raw in &self.additional_skill_paths {
            if !is_local_path(raw) {
                continue;
            }
            let resolved = self.resolve_resource_path(raw);
            if !resolved.exists()
                && !self
                    .skill_diagnostics
                    .iter()
                    .any(|d| d.path.as_deref() == Some(resolved.as_path()))
            {
                self.skill_diagnostics
                    .push(resource_error("Skill path does not exist", &resolved));
            }
        }
        self.rebuild_diagnostics();

        // --- Prompt templates (resource-loader.ts:434-451) ---
        let prompt_paths = if self.no_prompt_templates {
            let additional = self.resolve_additional_paths(&self.additional_prompt_template_paths);
            self.merge_paths(&[], &additional)
        } else {
            let discovered = discover_file_resource_paths(&FileResourceDiscovery {
                cwd: &self.cwd,
                agent_dir: &self.agent_dir,
                project_trusted: trusted,
                global_settings_entries: &global_prompt_entries,
                project_settings_entries: &project_prompt_entries,
                package_resources: &self.package_resources.prompt_paths,
                kind: FileResourceKind::Prompt,
            });
            let enabled: Vec<PathBuf> = discovered
                .into_iter()
                .filter(|r| r.enabled)
                .map(|r| r.path)
                .collect();
            let additional = self.resolve_additional_paths(&self.additional_prompt_template_paths);
            self.merge_paths(&enabled, &additional)
        };
        self.last_prompt_paths = prompt_paths.clone();
        self.update_prompts_from_paths(&prompt_paths);
        // resource-loader.ts:440-451: missing CLI prompt paths (isLocalPath-gated).
        for raw in &self.additional_prompt_template_paths {
            if !is_local_path(raw) {
                continue;
            }
            let resolved = self.resolve_resource_path(raw);
            if !resolved.exists()
                && !self
                    .prompt_diagnostics
                    .iter()
                    .any(|d| d.path.as_deref() == Some(resolved.as_path()))
            {
                self.prompt_diagnostics.push(resource_error(
                    "Prompt template path does not exist",
                    &resolved,
                ));
            }
        }
        self.rebuild_diagnostics();

        // --- Themes (resource-loader.ts:453-464) ---
        let theme_paths = if self.no_themes {
            let additional = self.resolve_additional_paths(&self.additional_theme_paths);
            self.merge_paths(&[], &additional)
        } else {
            let discovered = discover_file_resource_paths(&FileResourceDiscovery {
                cwd: &self.cwd,
                agent_dir: &self.agent_dir,
                project_trusted: trusted,
                global_settings_entries: &global_theme_entries,
                project_settings_entries: &project_theme_entries,
                package_resources: &self.package_resources.theme_paths,
                kind: FileResourceKind::Theme,
            });
            let enabled: Vec<PathBuf> = discovered
                .into_iter()
                .filter(|r| r.enabled)
                .map(|r| r.path)
                .collect();
            let additional = self.resolve_additional_paths(&self.additional_theme_paths);
            self.merge_paths(&enabled, &additional)
        };
        self.last_theme_paths = theme_paths.clone();
        self.update_themes_from_paths(&theme_paths);
        // resource-loader.ts:459-464: missing CLI theme paths — upstream does
        // NOT gate this loop on isLocalPath (unlike skills/prompts); kept as-is.
        for raw in &self.additional_theme_paths {
            let resolved = self.resolve_resource_path(raw);
            if !resolved.exists()
                && !self
                    .theme_diagnostics
                    .iter()
                    .any(|d| d.path.as_deref() == Some(resolved.as_path()))
            {
                self.theme_diagnostics
                    .push(resource_error("Theme path does not exist", &resolved));
            }
        }
        self.rebuild_diagnostics();

        // --- Context files (resource-loader.ts:466-475) — loaded regardless of trust ---
        self.resources.context_files = if self.no_context_files {
            Vec::new()
        } else {
            load_project_context_files(&self.cwd, &self.agent_dir)
        };

        // --- System prompt sources (resource-loader.ts:477-491) ---
        let system_prompt_input = match &self.system_prompt_source {
            Some(source) => Some(source.clone()),
            None => discover_system_prompt_file(&self.cwd, &self.agent_dir, trusted)
                .map(|p| p.to_string_lossy().into_owned()),
        };
        self.resources.system_prompt =
            resolve_prompt_input(system_prompt_input.as_deref(), "system prompt");

        let append_sources = match &self.append_system_prompt_source {
            Some(sources) => sources.clone(),
            None => discover_append_system_prompt_file(&self.cwd, &self.agent_dir, trusted)
                .map(|p| vec![p.to_string_lossy().into_owned()])
                .unwrap_or_default(),
        };
        self.resources.append_system_prompt = append_sources
            .iter()
            .filter_map(|s| resolve_prompt_input(Some(s), "append system prompt"))
            .collect();

        self.loaded = true;
    }

    /// `extendResources` (resource-loader.ts:293-331) — the
    /// `resources_discover` extension event hook (T15). Contributed paths
    /// merge into the last-loaded path sets and the affected resource types
    /// reload.
    pub fn extend_resources(&mut self, paths: &ResourceExtensionPaths) {
        let skill_entries: Vec<(PathBuf, SourceInfo)> = paths
            .skill_paths
            .iter()
            .map(|entry| self.normalize_extension_path(entry))
            .collect();
        let prompt_entries: Vec<(PathBuf, SourceInfo)> = paths
            .prompt_paths
            .iter()
            .map(|entry| self.normalize_extension_path(entry))
            .collect();
        let theme_entries: Vec<(PathBuf, SourceInfo)> = paths
            .theme_paths
            .iter()
            .map(|entry| self.normalize_extension_path(entry))
            .collect();

        self.extension_skill_source_infos
            .extend(skill_entries.iter().cloned());
        self.extension_prompt_source_infos
            .extend(prompt_entries.iter().cloned());
        self.extension_theme_source_infos
            .extend(theme_entries.iter().cloned());

        if !skill_entries.is_empty() {
            let new_paths: Vec<PathBuf> = skill_entries.iter().map(|(p, _)| p.clone()).collect();
            self.last_skill_paths = self.merge_paths(&self.last_skill_paths.clone(), &new_paths);
            let paths = self.last_skill_paths.clone();
            self.update_skills_from_paths(&paths, &[]);
        }
        if !prompt_entries.is_empty() {
            let new_paths: Vec<PathBuf> = prompt_entries.iter().map(|(p, _)| p.clone()).collect();
            self.last_prompt_paths = self.merge_paths(&self.last_prompt_paths.clone(), &new_paths);
            let paths = self.last_prompt_paths.clone();
            self.update_prompts_from_paths(&paths);
        }
        if !theme_entries.is_empty() {
            let new_paths: Vec<PathBuf> = theme_entries.iter().map(|(p, _)| p.clone()).collect();
            self.last_theme_paths = self.merge_paths(&self.last_theme_paths.clone(), &new_paths);
            let paths = self.last_theme_paths.clone();
            self.update_themes_from_paths(&paths);
        }
    }

    // -----------------------------------------------------------------------
    // Pipeline internals
    // -----------------------------------------------------------------------

    /// Extension placeholder (see header): resolve CLI extension paths and
    /// existence-check them (resource-loader.ts:408-415).
    fn reload_extensions(&mut self) {
        // Upstream `noExtensions` still loads CLI (`-e`) extension paths
        // (resource-loader.ts:403-405); the settings/auto/package discovery
        // it would disable is T14 scope, so the flag is inert in this slice.
        let _ = self.no_extensions;
        let mut loaded = LoadedExtensions::default();
        for raw in &self.additional_extension_paths {
            let resolved = self.resolve_resource_path(raw);
            if is_local_path(raw) && !resolved.exists() {
                loaded.errors.push(ExtensionLoadError {
                    error: format!("Extension path does not exist: {}", resolved.display()),
                    path: resolved,
                });
                continue;
            }
            loaded.paths.push(resolved);
        }
        self.resources.extensions = loaded;
    }

    /// The skills path set in load order: discovered (rank-sorted, enabled
    /// only) → packages (rank 4) → CLI `--skill` paths
    /// (resource-loader.ts:419-421). Returns the paths plus the provenance
    /// map used to refine skill `SourceInfo` (resource-loader.ts:631-637).
    fn compute_skill_paths(
        &self,
        trusted: bool,
        global_entries: &[String],
        project_entries: &[String],
    ) -> (Vec<PathBuf>, Vec<(PathBuf, SourceInfo)>) {
        if self.no_skills {
            // resource-loader.ts:420: mergePaths(cliEnabledSkills, additionalSkillPaths);
            // cliEnabledSkills is empty in this slice (extension scanning is T14).
            let additional = self.resolve_additional_paths(&self.additional_skill_paths);
            return (self.merge_paths(&[], &additional), Vec::new());
        }

        let discovered = discover_skill_paths(&DiscoverSkillsOptions {
            cwd: self.cwd.clone(),
            agent_dir: self.agent_dir.clone(),
            home_dir: self.home_dir.clone().or_else(config::user_home_dir),
            project_trusted: trusted,
            global_settings_skills: global_entries.to_vec(),
            project_settings_skills: project_entries.to_vec(),
            // CLI paths are appended below, after packages (upstream order);
            // discover_skill_paths would put them last itself.
            cli_skill_paths: Vec::new(),
        });

        let mut paths: Vec<PathBuf> = Vec::new();
        let mut source_map: Vec<(PathBuf, SourceInfo)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for resource in &discovered {
            if !resource.enabled {
                continue;
            }
            source_map.push((
                resource.path.clone(),
                create_source_info(&resource.path, &resource.metadata),
            ));
            if seen.insert(canonicalize_path(&resource.path)) {
                paths.push(resource.path.clone());
            }
        }

        // Packages input port (T14): precedence rank 4 — after every
        // discovered path, before CLI paths.
        for package in &self.package_resources.skill_paths {
            if !package.enabled {
                continue;
            }
            source_map.push((package.path.clone(), package_source_info(package)));
            if seen.insert(canonicalize_path(&package.path)) {
                paths.push(package.path.clone());
            }
        }

        for raw in &self.additional_skill_paths {
            let resolved = self.resolve_resource_path(raw);
            if seen.insert(canonicalize_path(&resolved)) {
                paths.push(resolved);
            }
        }

        (paths, source_map)
    }

    /// `updateSkillsFromPaths` (resource-loader.ts:618-639).
    fn update_skills_from_paths(
        &mut self,
        skill_paths: &[PathBuf],
        metadata_source_map: &[(PathBuf, SourceInfo)],
    ) {
        let mut result: LoadSkillsResult = if self.no_skills && skill_paths.is_empty() {
            LoadSkillsResult::default()
        } else {
            load_skills(&LoadSkillsOptions {
                cwd: self.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                skill_paths: skill_paths.to_vec(),
                include_defaults: false,
            })
        };
        for skill in &mut result.skills {
            if let Some(info) = find_source_info_for_path(
                &skill.file_path,
                &self.extension_skill_source_infos,
                metadata_source_map,
            ) {
                skill.source_info = info;
            }
        }
        self.resources.skills = result.skills;
        self.skill_diagnostics = result.diagnostics;
        self.rebuild_diagnostics();
    }

    /// `updatePromptsFromPaths` (resource-loader.ts:641-663), minus
    /// sourceInfo (not carried by the Rust `PromptTemplate`).
    fn update_prompts_from_paths(&mut self, prompt_paths: &[PathBuf]) {
        let (prompts, diagnostics) = if self.no_prompt_templates && prompt_paths.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let loaded = load_prompt_templates(&LoadPromptTemplatesOptions {
                cwd: self.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                prompt_paths: prompt_paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                include_defaults: false,
            });
            dedupe_prompts(loaded)
        };
        self.resources.prompts = prompts;
        self.prompt_diagnostics = diagnostics;
        self.rebuild_diagnostics();
    }

    /// `updateThemesFromPaths` (resource-loader.ts:665-685), minus
    /// sourceInfo (not carried by the Rust `Theme`).
    fn update_themes_from_paths(&mut self, theme_paths: &[PathBuf]) {
        let (themes, diagnostics) = if self.no_themes && theme_paths.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let (loaded, mut load_diagnostics) = self.load_themes(theme_paths);
            let (deduped, dedupe_diagnostics) = dedupe_themes(loaded);
            load_diagnostics.extend(dedupe_diagnostics);
            (deduped, load_diagnostics)
        };
        self.resources.themes = themes;
        self.theme_diagnostics = diagnostics;
        self.rebuild_diagnostics();
    }

    /// `loadThemes` (resource-loader.ts:811-851) with
    /// `includeDefaults: false` — the default theme dirs are covered by
    /// discovery, so only the given paths load here.
    fn load_themes(&self, paths: &[PathBuf]) -> (Vec<Theme>, Vec<ResourceDiagnostic>) {
        let mut themes = Vec::new();
        let mut diagnostics = Vec::new();
        for path in paths {
            let resolved = self.resolve_resource_path(&path.to_string_lossy());
            if !resolved.exists() {
                diagnostics.push(resource_warning("theme path does not exist", &resolved));
                continue;
            }
            match std::fs::metadata(&resolved) {
                Ok(metadata) if metadata.is_dir() => {
                    load_themes_from_dir(&resolved, &mut themes, &mut diagnostics);
                }
                Ok(metadata)
                    if metadata.is_file() && resolved.to_string_lossy().ends_with(".json") =>
                {
                    load_theme_from_file(&resolved, &mut themes, &mut diagnostics);
                }
                Ok(_) => {
                    diagnostics.push(resource_warning("theme path is not a json file", &resolved));
                }
                Err(error) => {
                    diagnostics.push(resource_warning(error.to_string(), &resolved));
                }
            }
        }
        (themes, diagnostics)
    }

    /// `mergePaths` (resource-loader.ts:792-805): concatenate, resolve each
    /// entry against `cwd`, dedupe by canonical path (first occurrence wins).
    fn merge_paths(&self, primary: &[PathBuf], additional: &[PathBuf]) -> Vec<PathBuf> {
        let mut merged = Vec::new();
        let mut seen = HashSet::new();
        for path in primary.iter().chain(additional) {
            let resolved = self.resolve_resource_path(&path.to_string_lossy());
            if !seen.insert(canonicalize_path(&resolved)) {
                continue;
            }
            merged.push(resolved);
        }
        merged
    }

    /// `resolveResourcePath` (resource-loader.ts:807-809):
    /// `resolvePath(p, cwd, { trim: true })`.
    fn resolve_resource_path(&self, p: &str) -> PathBuf {
        resolve_path(p.trim(), &self.cwd)
    }

    fn resolve_additional_paths(&self, raws: &[String]) -> Vec<PathBuf> {
        raws.iter()
            .map(|raw| self.resolve_resource_path(raw))
            .collect()
    }

    /// `normalizeExtensionPaths` (resource-loader.ts:604-616): resolve the
    /// contributed path and the source info's `base_dir` against `cwd`.
    fn normalize_extension_path(&self, entry: &ResourceExtensionPath) -> (PathBuf, SourceInfo) {
        let resolved = self.resolve_resource_path(&entry.path.to_string_lossy());
        let mut info = entry.source_info.clone();
        info.path = resolved.clone();
        info.base_dir = info
            .base_dir
            .map(|base| self.resolve_resource_path(&base.to_string_lossy()));
        (resolved, info)
    }

    fn rebuild_diagnostics(&mut self) {
        let mut diagnostics = Vec::new();
        diagnostics.extend(self.skill_diagnostics.iter().cloned());
        diagnostics.extend(self.prompt_diagnostics.iter().cloned());
        diagnostics.extend(self.theme_diagnostics.iter().cloned());
        self.resources.diagnostics = diagnostics;
    }
}

// ---------------------------------------------------------------------------
// Diagnostics helpers
// ---------------------------------------------------------------------------

fn resource_warning(message: impl Into<String>, path: &Path) -> ResourceDiagnostic {
    ResourceDiagnostic {
        kind: DiagnosticKind::Warning,
        message: message.into(),
        path: Some(path.to_path_buf()),
        collision: None,
    }
}

fn resource_error(message: impl Into<String>, path: &Path) -> ResourceDiagnostic {
    ResourceDiagnostic {
        kind: DiagnosticKind::Error,
        message: message.into(),
        path: Some(path.to_path_buf()),
        collision: None,
    }
}

// ---------------------------------------------------------------------------
// Source info (source-info.ts + resource-loader.ts:701-790)
// ---------------------------------------------------------------------------

/// `createSourceInfo` (source-info.ts:15-23) for discovered resources.
fn create_source_info(path: &Path, metadata: &PathMetadata) -> SourceInfo {
    SourceInfo {
        path: path.to_path_buf(),
        source: match metadata.source {
            MetadataSource::Local => "local".to_string(),
            MetadataSource::Auto => "auto".to_string(),
        },
        scope: metadata.scope,
        origin: metadata.origin,
        base_dir: metadata.base_dir.clone(),
    }
}

/// Source info for package resources (T14 input port). Upstream carries the
/// package source string (`"npm"`/`"git"`/…, package-manager.ts:1252); this
/// slice uses the generic `"package"` label until T14 threads real sources.
fn package_source_info(package: &PackageResource) -> SourceInfo {
    SourceInfo {
        path: package.path.clone(),
        source: "package".to_string(),
        scope: package.scope,
        origin: SourceOrigin::Package,
        base_dir: package.base_dir.clone(),
    }
}

/// `findSourceInfoForPath` (resource-loader.ts:701-745): exact-or-ancestor
/// match against the `resources_discover` contributions first, then against
/// the discovery provenance map. `resourcePath`s in `<...>` form (inline
/// factories) do not occur in this slice and yield `None`.
fn find_source_info_for_path(
    resource_path: &Path,
    extra_source_infos: &[(PathBuf, SourceInfo)],
    metadata_source_map: &[(PathBuf, SourceInfo)],
) -> Option<SourceInfo> {
    if resource_path.as_os_str().is_empty() {
        return None;
    }
    if resource_path.to_string_lossy().starts_with('<') {
        return None;
    }

    let normalized = node_resolve(resource_path);
    for (source_path, info) in extra_source_infos {
        let normalized_source = node_resolve(source_path);
        if normalized == normalized_source || normalized.starts_with(&normalized_source) {
            return Some(SourceInfo {
                path: resource_path.to_path_buf(),
                ..info.clone()
            });
        }
    }

    // Exact match first (upstream `metadataByPath.get(normalized) ?? .get(raw)`).
    for (source_path, info) in metadata_source_map {
        if normalized == node_resolve(source_path) || resource_path == source_path {
            return Some(SourceInfo {
                path: resource_path.to_path_buf(),
                ..info.clone()
            });
        }
    }
    for (source_path, info) in metadata_source_map {
        let normalized_source = node_resolve(source_path);
        if normalized == normalized_source || normalized.starts_with(&normalized_source) {
            return Some(SourceInfo {
                path: resource_path.to_path_buf(),
                ..info.clone()
            });
        }
    }

    None
}

/// Node `resolve()` for inputs that are absolute in practice: normalize
/// lexically, resolving relative inputs against the process cwd.
fn node_resolve(path: &Path) -> PathBuf {
    let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    resolve_path(&path.to_string_lossy(), &process_cwd)
}

// ---------------------------------------------------------------------------
// Dedupe (resource-loader.ts:916-967)
// ---------------------------------------------------------------------------

/// `dedupePrompts` (resource-loader.ts:916-940): first occurrence of a name
/// wins; later duplicates produce `collision` diagnostics.
fn dedupe_prompts(prompts: Vec<PromptTemplate>) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics = Vec::new();

    for prompt in prompts {
        if seen.contains(&prompt.name) {
            let winner_path = kept
                .iter()
                .find(|p| p.name == prompt.name)
                .map(|p| p.file_path.clone())
                .unwrap_or_default();
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("name \"/{}\" collision", prompt.name),
                path: Some(prompt.file_path.clone()),
                collision: Some(ResourceCollision {
                    resource_type: DiagnosticResourceType::Prompt,
                    name: prompt.name.clone(),
                    winner_path,
                    loser_path: prompt.file_path.clone(),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            seen.insert(prompt.name.clone());
            kept.push(prompt);
        }
    }

    (kept, diagnostics)
}

/// `dedupeThemes` (resource-loader.ts:942-967): first occurrence of a name
/// (`name ?? "unnamed"`) wins; missing source paths render as `<builtin>`.
fn dedupe_themes(themes: Vec<Theme>) -> (Vec<Theme>, Vec<ResourceDiagnostic>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<Theme> = Vec::new();
    let mut diagnostics = Vec::new();

    for theme in themes {
        let name = theme.name.clone().unwrap_or_else(|| "unnamed".to_string());
        if seen.contains(&name) {
            let winner_path = kept
                .iter()
                .find(|t| (t.name.clone().unwrap_or_else(|| "unnamed".to_string())) == name)
                .and_then(|t| t.source_path.clone())
                .unwrap_or_else(|| PathBuf::from("<builtin>"));
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("name \"{name}\" collision"),
                path: theme.source_path.clone(),
                collision: Some(ResourceCollision {
                    resource_type: DiagnosticResourceType::Theme,
                    name: name.clone(),
                    winner_path,
                    loser_path: theme
                        .source_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("<builtin>")),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            seen.insert(name);
            kept.push(theme);
        }
    }

    (kept, diagnostics)
}

// ---------------------------------------------------------------------------
// Theme loading helpers (resource-loader.ts:853-890)
// ---------------------------------------------------------------------------

/// `loadThemesFromDir` (resource-loader.ts:853-881): flat `.json` scan,
/// symlinks followed, broken links skipped.
fn load_themes_from_dir(
    dir: &Path,
    themes: &mut Vec<Theme>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    if !dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(resource_warning(error.to_string(), dir));
            return;
        }
    };
    for entry in entries.flatten() {
        let full_path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let is_file = if file_type.is_symlink() {
            match std::fs::metadata(&full_path) {
                Ok(metadata) => metadata.is_file(),
                Err(_) => continue,
            }
        } else {
            file_type.is_file()
        };
        if is_file && entry.file_name().to_string_lossy().ends_with(".json") {
            load_theme_from_file(&full_path, themes, diagnostics);
        }
    }
}

/// `loadThemeFromFile` (resource-loader.ts:883-890): load failures degrade
/// to warning diagnostics.
fn load_theme_from_file(
    path: &Path,
    themes: &mut Vec<Theme>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) {
    match load_theme_from_path(path, None) {
        Ok(theme) => themes.push(theme),
        Err(error) => {
            // Record the bare message, like upstream `error.message` — the
            // `PirError` Display prefix ("resource loading error: ") is not
            // part of the diagnostic text.
            let message = match &error {
                PirError::Resource(inner)
                | PirError::Settings(inner)
                | PirError::Session(inner) => inner.clone(),
                other => other.to_string(),
            };
            diagnostics.push(resource_warning(message, path));
        }
    }
}

// ---------------------------------------------------------------------------
// Prompts/themes discovery (package-manager.ts subset)
// ---------------------------------------------------------------------------

/// The file-based resource kinds discovered by this module: prompt
/// templates (`*.md`) and themes (`*.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileResourceKind {
    Prompt,
    Theme,
}

impl FileResourceKind {
    /// `FILE_PATTERNS` (package-manager.ts:202-205).
    fn extension(self) -> &'static str {
        match self {
            FileResourceKind::Prompt => ".md",
            FileResourceKind::Theme => ".json",
        }
    }
}

/// One resolved prompts/themes path with enablement and provenance
/// (upstream `ResolvedResource`).
#[derive(Debug, Clone)]
struct ResolvedFileResourcePath {
    path: PathBuf,
    enabled: bool,
    metadata: PathMetadata,
}

/// Inputs for [`discover_file_resource_paths`].
struct FileResourceDiscovery<'a> {
    cwd: &'a Path,
    agent_dir: &'a Path,
    /// Trust gate for project-local auto discovery (`.pir/prompts`,
    /// `.pir/themes`). Project settings entries arrive empty from an
    /// untrusted `SettingsManager`, so they need no extra gate.
    project_trusted: bool,
    global_settings_entries: &'a [String],
    project_settings_entries: &'a [String],
    package_resources: &'a [PackageResource],
    kind: FileResourceKind,
}

/// The prompts/themes subset of `PackageManager.resolve`
/// (package-manager.ts:901-953) plus `addAutoDiscoveredResources`
/// (:2303-2467) and `toResolvedPaths` (:2527-2545): collect settings entries
/// and auto-discovered paths, tag them with precedence metadata, stable-sort
/// by [`resource_precedence_rank`] and dedupe by canonical path (first
/// occurrence wins). Package resources join first in the accumulator
/// (upstream resolves packages before any local/auto entries) and sort to
/// rank 4. CLI paths are merged afterwards by the caller.
fn discover_file_resource_paths(options: &FileResourceDiscovery) -> Vec<ResolvedFileResourcePath> {
    let cwd = node_resolve(options.cwd);
    let agent_dir = node_resolve(options.agent_dir);
    let global_base_dir = agent_dir.clone();
    let project_base_dir = config::get_project_config_dir(&cwd);
    let (user_dir, project_dir) = match options.kind {
        FileResourceKind::Prompt => (
            agent_dir.join("prompts"),
            config::get_project_prompts_dir(&cwd),
        ),
        FileResourceKind::Theme => (
            agent_dir.join("themes"),
            config::get_project_themes_dir(&cwd),
        ),
    };

    // Accumulator with `addResource` semantics (package-manager.ts:2506-2516):
    // insertion-ordered, first write for a given raw path wins.
    let mut accumulator: Vec<ResolvedFileResourcePath> = Vec::new();
    let mut seen_raw: HashSet<String> = HashSet::new();
    macro_rules! add {
        ($entry:expr) => {{
            let entry: ResolvedFileResourcePath = $entry;
            if !entry.path.as_os_str().is_empty()
                && seen_raw.insert(entry.path.to_string_lossy().into_owned())
            {
                accumulator.push(entry);
            }
        }};
    }

    // Packages first (package-manager.ts:917: resolvePackageSources runs
    // before local entries and auto discovery).
    for package in options.package_resources {
        add!(ResolvedFileResourcePath {
            path: package.path.clone(),
            enabled: package.enabled,
            metadata: PathMetadata {
                source: MetadataSource::Auto,
                scope: package.scope,
                origin: SourceOrigin::Package,
                base_dir: package.base_dir.clone(),
            },
        });
    }

    // Settings entries: project before global (package-manager.ts:922-948).
    let project_local = PathMetadata {
        source: MetadataSource::Local,
        scope: SourceScope::Project,
        origin: SourceOrigin::TopLevel,
        base_dir: None,
    };
    for entry in resolve_local_file_entries(
        options.project_settings_entries,
        &project_base_dir,
        &project_local,
        options.kind,
    ) {
        add!(entry);
    }
    let user_local = PathMetadata {
        source: MetadataSource::Local,
        scope: SourceScope::User,
        origin: SourceOrigin::TopLevel,
        base_dir: None,
    };
    for entry in resolve_local_file_entries(
        options.global_settings_entries,
        &global_base_dir,
        &user_local,
        options.kind,
    ) {
        add!(entry);
    }

    // Auto-discovered project resources (trust-gated, package-manager.ts:2401-2418).
    if options.project_trusted {
        let metadata = PathMetadata {
            source: MetadataSource::Auto,
            scope: SourceScope::Project,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(project_base_dir.clone()),
        };
        for path in collect_auto_file_entries(&project_dir, options.kind) {
            let enabled =
                is_enabled_by_overrides(&path, options.project_settings_entries, &project_base_dir);
            add!(ResolvedFileResourcePath {
                path,
                enabled,
                metadata: metadata.clone(),
            });
        }
    }

    // Auto-discovered user resources (package-manager.ts:2455-2466).
    let user_metadata = PathMetadata {
        source: MetadataSource::Auto,
        scope: SourceScope::User,
        origin: SourceOrigin::TopLevel,
        base_dir: Some(global_base_dir.clone()),
    };
    for path in collect_auto_file_entries(&user_dir, options.kind) {
        let enabled =
            is_enabled_by_overrides(&path, options.global_settings_entries, &global_base_dir);
        add!(ResolvedFileResourcePath {
            path,
            enabled,
            metadata: user_metadata.clone(),
        });
    }

    // `toResolvedPaths` (:2527-2545): stable rank sort, then canonical-path
    // dedupe (first occurrence wins).
    let mut indexed: Vec<(usize, ResolvedFileResourcePath)> =
        accumulator.into_iter().enumerate().collect();
    indexed.sort_by_key(|(index, entry)| (resource_precedence_rank(&entry.metadata), *index));
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    let mut resolved = Vec::new();
    for (_, entry) in indexed {
        if seen_canonical.insert(canonicalize_path(&entry.path)) {
            resolved.push(entry);
        }
    }
    resolved
}

/// `resolveLocalEntries` (package-manager.ts:2280-2301) for prompts/themes:
/// plain entries resolve against `base_dir` (files taken as-is, directories
/// scanned recursively via `collectFiles`); pattern entries only filter
/// enablement via [`apply_patterns`].
fn resolve_local_file_entries(
    entries: &[String],
    base_dir: &Path,
    metadata: &PathMetadata,
    kind: FileResourceKind,
) -> Vec<ResolvedFileResourcePath> {
    if entries.is_empty() {
        return Vec::new();
    }
    let (plain, patterns) = split_patterns(entries);
    // `collectFilesFromPaths` (package-manager.ts:2469-2486).
    let mut all_files = Vec::new();
    for entry in &plain {
        let resolved = resolve_path(entry.trim(), base_dir);
        if resolved.is_file() {
            all_files.push(resolved);
        } else if resolved.is_dir() {
            all_files.extend(collect_files_recursive(&resolved, kind));
        }
    }
    let enabled = apply_patterns(&all_files, &patterns, base_dir);
    all_files
        .into_iter()
        .map(|path| ResolvedFileResourcePath {
            enabled: enabled.contains(&path),
            path,
            metadata: metadata.clone(),
        })
        .collect()
}

/// `collectAutoPromptEntries` / `collectAutoThemeEntries`
/// (package-manager.ts:462-530): a **flat** scan of `dir` collecting files
/// with the kind's extension. Dot-entries and `node_modules` are skipped,
/// `.gitignore`/`.ignore`/`.fdignore` rules apply, symlinks are followed.
fn collect_auto_file_entries(dir: &Path, kind: FileResourceKind) -> Vec<PathBuf> {
    file_walk(dir, kind, Some(1))
}

/// `collectFiles` (package-manager.ts:301-346) for prompts/themes: a
/// **recursive** scan with the same skip/ignore/symlink rules, used for
/// settings/CLI plain directory entries.
fn collect_files_recursive(dir: &Path, kind: FileResourceKind) -> Vec<PathBuf> {
    file_walk(dir, kind, None)
}

/// Shared walker behind [`collect_auto_file_entries`] and
/// [`collect_files_recursive`], configured exactly like
/// `skills::collect_skill_entries` (ignore-crate `WalkBuilder`).
fn file_walk(dir: &Path, kind: FileResourceKind, max_depth: Option<usize>) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }
    let extension = kind.extension();

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
        .add_custom_ignore_filename(".fdignore")
        .filter_entry(|entry| entry.file_name() != "node_modules");
    if let Some(depth) = max_depth {
        builder.max_depth(Some(depth));
    }

    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == dir {
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.file_name().to_string_lossy().ends_with(extension) {
            entries.push(path.to_path_buf());
        }
    }
    entries
}

/// `isPattern` (package-manager.ts:271-273).
fn is_pattern(s: &str) -> bool {
    s.starts_with('!')
        || s.starts_with('+')
        || s.starts_with('-')
        || s.contains('*')
        || s.contains('?')
}

/// `splitPatterns` (package-manager.ts:283-299). (skills.rs keeps its own
/// private copy for the skills pipeline; this is the package-manager copy
/// for prompts/themes.)
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

/// `isLocalPath` (paths.ts:41-56): false for package-source/URL prefixes;
/// bare names, relative paths and `file:` URLs are local.
fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:"))
}

/// `[...(settings[key] ?? [])]` on a per-scope settings object
/// (settings-manager.ts:970 ff.): a non-array value reads as empty,
/// non-string items are dropped.
fn settings_string_array(settings: &Settings, key: &str) -> Vec<String> {
    settings
        .as_map()
        .get(key)
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Keybindings config-file migration (migrations.ts:157-172)
// ---------------------------------------------------------------------------

/// `migrateKeybindingsConfigFile` (migrations.ts:157-172): rewrite the
/// global `keybindings.json` in place when legacy key names were migrated.
/// Returns whether a write happened.
///
/// Unlike upstream (unlocked `writeFileSync`), the write runs under an fs2
/// flock on the file, matching the settings-manager locking discipline
/// (coding-standards §9.2). Malformed content is ignored silently, exactly
/// like upstream; lock and write I/O failures propagate.
pub fn migrate_keybindings_config_file() -> Result<bool, PirError> {
    migrate_keybindings_config_file_at(&config::get_keybindings_path())
}

/// [`migrate_keybindings_config_file`] against an explicit path (tests and
/// custom agent dirs).
pub fn migrate_keybindings_config_file_at(path: &Path) -> Result<bool, PirError> {
    if !path.exists() {
        return Ok(false);
    }

    // `acquireLockSyncWithRetry` discipline (settings-manager.ts:199-224):
    // exclusive flock with a bounded retry budget; released explicitly below.
    let mut file = None;
    for attempt in 1..=LOCK_MAX_ATTEMPTS {
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        match opened.try_lock_exclusive() {
            Ok(()) => {
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt == LOCK_MAX_ATTEMPTS {
                    return Err(PirError::Resource(format!(
                        "Failed to acquire keybindings lock for {}: {error}",
                        path.display()
                    )));
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(PirError::Io(error)),
        }
    }
    let Some(file) = file else {
        return Err(PirError::Resource(format!(
            "Failed to acquire keybindings lock for {}",
            path.display()
        )));
    };

    let result = migrate_locked_keybindings(path, &file);
    // Explicit release mirroring upstream's `finally { release() }`.
    let _ = file.unlock();
    result
}

fn migrate_locked_keybindings(path: &Path, file: &std::fs::File) -> Result<bool, PirError> {
    let content = std::fs::read_to_string(path)?;
    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(parsed) => parsed,
        // Malformed JSON is ignored, exactly like upstream's catch-all.
        Err(_) => return Ok(false),
    };
    let Some(raw_map) = parsed.as_object().cloned() else {
        return Ok(false);
    };
    let (migrated_config, migrated) = migrate_keybindings_config(&raw_map);
    if !migrated {
        return Ok(false);
    }
    // Upstream writes `JSON.stringify(config, null, 2) + "\n"`;
    // serde_json's pretty printer uses the same two-space indent.
    let mut output = serde_json::to_string_pretty(&Value::Object(migrated_config))?;
    output.push('\n');
    file.set_len(0)?;
    let mut handle = file;
    handle.seek(SeekFrom::Start(0))?;
    handle.write_all(output.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::keybindings::KEYBINDING_NAME_MIGRATIONS;
    use crate::core::settings_manager::SettingsManagerCreateOptions;
    use crate::core::themes::{create_theme, parse_theme_json, REQUIRED_COLOR_KEYS};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "pir-resource-loader-test-{}-{nanos}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("fixture path has parent"))
            .expect("create fixture parent dirs");
        std::fs::write(path, content).expect("write fixture file");
    }

    fn prompt(name: &str, file_path: &Path) -> PromptTemplate {
        PromptTemplate {
            name: name.to_string(),
            description: String::new(),
            argument_hint: None,
            content: String::new(),
            file_path: file_path.to_path_buf(),
        }
    }

    // --- is_local_path (paths.ts:41-56) -------------------------------------

    #[test]
    fn test_is_local_path() {
        assert!(is_local_path("./foo"));
        assert!(is_local_path("/abs/path"));
        assert!(is_local_path("file:///tmp/x"));
        assert!(is_local_path("  ./spaced  "));
        assert!(!is_local_path("npm:package"));
        assert!(!is_local_path("git:github.com/foo/bar"));
        assert!(!is_local_path("github:foo/bar"));
        assert!(!is_local_path("https://example.com/x.git"));
        assert!(!is_local_path("http://example.com"));
        assert!(!is_local_path("ssh://git@example.com"));
    }

    // --- split_patterns (package-manager.ts:283-299) -------------------------

    #[test]
    fn test_split_patterns() {
        let entries = vec![
            "skills/foo".to_string(),
            "!skip.md".to_string(),
            "+force.md".to_string(),
            "-ban.md".to_string(),
            "glob-*".to_string(),
            "que?".to_string(),
        ];
        let (plain, patterns) = split_patterns(&entries);
        assert_eq!(plain, vec!["skills/foo".to_string()]);
        assert_eq!(
            patterns,
            vec![
                "!skip.md".to_string(),
                "+force.md".to_string(),
                "-ban.md".to_string(),
                "glob-*".to_string(),
                "que?".to_string()
            ]
        );
    }

    // --- merge_paths (resource-loader.ts:792-805) ----------------------------

    #[test]
    fn test_merge_paths_dedupes_canonical_first_wins() {
        let tmp = TempDir::new();
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let file_a = cwd.join("a.md");
        write(&file_a, "a");

        let loader = DefaultResourceLoader::new(DefaultResourceLoaderOptions::new(
            cwd.clone(),
            tmp.path().join("agent"),
        ));
        let merged = loader.merge_paths(
            &[PathBuf::from("a.md"), file_a.clone()],
            &[cwd.join("./a.md")],
        );
        assert_eq!(merged, vec![file_a]);
    }

    // --- dedupe_prompts (resource-loader.ts:916-940) -------------------------

    #[test]
    fn test_dedupe_prompts_first_wins_with_collision_diagnostic() {
        let first = prompt("commit", Path::new("/user/prompts/commit.md"));
        let second = prompt("commit", Path::new("/project/.pir/prompts/commit.md"));
        let third = prompt("review", Path::new("/user/prompts/review.md"));

        let (prompts, diagnostics) = dedupe_prompts(vec![first, second, third]);
        assert_eq!(prompts.len(), 2);
        assert_eq!(
            prompts[0].file_path,
            PathBuf::from("/user/prompts/commit.md")
        );
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.kind, DiagnosticKind::Collision);
        assert_eq!(diagnostic.message, "name \"/commit\" collision");
        assert_eq!(
            diagnostic.path.as_deref(),
            Some(Path::new("/project/.pir/prompts/commit.md"))
        );
        let collision = diagnostic.collision.as_ref().expect("collision payload");
        assert_eq!(collision.resource_type, DiagnosticResourceType::Prompt);
        assert_eq!(collision.name, "commit");
        assert_eq!(
            collision.winner_path,
            PathBuf::from("/user/prompts/commit.md")
        );
        assert_eq!(
            collision.loser_path,
            PathBuf::from("/project/.pir/prompts/commit.md")
        );
    }

    // --- dedupe_themes (resource-loader.ts:942-967) --------------------------

    fn theme_with_colors(name: &str, source_path: Option<&Path>) -> Theme {
        let colors: serde_json::Map<String, Value> = REQUIRED_COLOR_KEYS
            .iter()
            .map(|key| (key.to_string(), Value::String("#000000".to_string())))
            .collect();
        let value = serde_json::json!({ "name": name, "colors": Value::Object(colors) });
        let parsed = parse_theme_json("test", &value).expect("theme json");
        create_theme(&parsed, None, source_path).expect("theme")
    }

    #[test]
    fn test_dedupe_themes_first_wins_and_builtin_paths() {
        let first = theme_with_colors("dup", None);
        let second = theme_with_colors("dup", Some(Path::new("/user/themes/dup.json")));
        let third = theme_with_colors("solo", Some(Path::new("/user/themes/solo.json")));

        let (themes, diagnostics) = dedupe_themes(vec![first, second, third]);
        assert_eq!(themes.len(), 2);
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.kind, DiagnosticKind::Collision);
        assert_eq!(diagnostic.message, "name \"dup\" collision");
        assert_eq!(
            diagnostic.path.as_deref(),
            Some(Path::new("/user/themes/dup.json"))
        );
        let collision = diagnostic.collision.as_ref().expect("collision payload");
        assert_eq!(collision.resource_type, DiagnosticResourceType::Theme);
        // Winner has no source path → `<builtin>` placeholder.
        assert_eq!(collision.winner_path, PathBuf::from("<builtin>"));
        assert_eq!(collision.loser_path, PathBuf::from("/user/themes/dup.json"));
    }

    // --- collect / resolve / discover (package-manager.ts subset) ------------

    #[test]
    fn test_collect_auto_file_entries_flat_and_filtered() {
        let tmp = TempDir::new();
        let dir = tmp.path().join("prompts");
        write(&dir.join("a.md"), "a");
        write(&dir.join("b.txt"), "b");
        write(&dir.join(".hidden.md"), "hidden");
        write(&dir.join("sub").join("nested.md"), "nested");
        write(&dir.join("node_modules").join("dep.md"), "dep");
        write(&dir.join("ignored.md"), "ignored");
        write(&dir.join(".gitignore"), "ignored.md\n");

        let entries = collect_auto_file_entries(&dir, FileResourceKind::Prompt);
        assert_eq!(entries, vec![dir.join("a.md")]);

        // Recursive variant picks up the nested file too.
        let recursive = collect_files_recursive(&dir, FileResourceKind::Prompt);
        assert!(recursive.contains(&dir.join("a.md")));
        assert!(recursive.contains(&dir.join("sub").join("nested.md")));
        assert!(!recursive.contains(&dir.join("ignored.md")));
        assert!(!recursive.contains(&dir.join("node_modules").join("dep.md")));

        // Themes kind only collects .json.
        write(&dir.join("theme.json"), "{}");
        let themes = collect_auto_file_entries(&dir, FileResourceKind::Theme);
        assert_eq!(themes, vec![dir.join("theme.json")]);
    }

    #[test]
    fn test_resolve_local_file_entries_patterns_filter_enablement() {
        let tmp = TempDir::new();
        let base = tmp.path().join("base");
        write(&base.join("prompts").join("keep.md"), "keep");
        write(&base.join("prompts").join("skip.md"), "skip");
        let metadata = PathMetadata {
            source: MetadataSource::Local,
            scope: SourceScope::User,
            origin: SourceOrigin::TopLevel,
            base_dir: None,
        };

        // Plain dir entry + force-exclude pattern.
        let entries = resolve_local_file_entries(
            &["prompts".to_string(), "-prompts/skip.md".to_string()],
            &base,
            &metadata,
            FileResourceKind::Prompt,
        );
        let by_name = |name: &str| {
            entries
                .iter()
                .find(|e| e.path.file_name().expect("file name").to_string_lossy() == name)
                .expect("entry present")
        };
        assert!(by_name("keep.md").enabled);
        assert!(!by_name("skip.md").enabled);

        // Glob pattern as sole entry: no plain entries → nothing collected.
        let glob_only = resolve_local_file_entries(
            &["!prompts/skip.md".to_string()],
            &base,
            &metadata,
            FileResourceKind::Prompt,
        );
        assert!(glob_only.is_empty());
    }

    #[test]
    fn test_discover_file_resource_paths_rank_sort_and_trust_gate() {
        let tmp = TempDir::new();
        let cwd = tmp.path().join("project");
        let agent_dir = tmp.path().join("agent");
        // user auto (rank 3)
        write(&agent_dir.join("prompts").join("auto-user.md"), "user");
        // project auto (rank 1)
        write(
            &cwd.join(".pir").join("prompts").join("auto-project.md"),
            "project",
        );
        // settings entries (rank 0 project, rank 2 user)
        let project_settings_dir = tmp.path().join("ps");
        write(&project_settings_dir.join("settings-project.md"), "ps");
        let user_settings_dir = tmp.path().join("us");
        write(&user_settings_dir.join("settings-user.md"), "us");

        let discovery = FileResourceDiscovery {
            cwd: &cwd,
            agent_dir: &agent_dir,
            project_trusted: true,
            global_settings_entries: &[user_settings_dir.to_string_lossy().into_owned()],
            project_settings_entries: &[project_settings_dir.to_string_lossy().into_owned()],
            package_resources: &[],
            kind: FileResourceKind::Prompt,
        };
        let resolved = discover_file_resource_paths(&discovery);
        let names: Vec<String> = resolved
            .iter()
            .map(|r| {
                r.path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "settings-project.md",
                "auto-project.md",
                "settings-user.md",
                "auto-user.md"
            ]
        );

        // Untrusted: project auto discovery drops out (project settings
        // entries would arrive empty from an untrusted SettingsManager).
        let discovery = FileResourceDiscovery {
            project_trusted: false,
            project_settings_entries: &[],
            ..discovery
        };
        let resolved = discover_file_resource_paths(&discovery);
        let names: Vec<String> = resolved
            .iter()
            .map(|r| {
                r.path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["settings-user.md", "auto-user.md"]);
    }

    #[test]
    fn test_discover_file_resource_paths_package_rank_and_dedupe() {
        let tmp = TempDir::new();
        let cwd = tmp.path().join("project");
        let agent_dir = tmp.path().join("agent");
        let shared = agent_dir.join("prompts").join("shared.md");
        write(&shared, "shared");
        write(&agent_dir.join("prompts").join("auto.md"), "auto");

        let package = PackageResource {
            path: shared.clone(),
            enabled: true,
            scope: SourceScope::User,
            base_dir: None,
        };
        let discovery = FileResourceDiscovery {
            cwd: &cwd,
            agent_dir: &agent_dir,
            project_trusted: true,
            global_settings_entries: &[],
            project_settings_entries: &[],
            package_resources: &[package],
            kind: FileResourceKind::Prompt,
        };
        let resolved = discover_file_resource_paths(&discovery);
        // Raw-path first-write-wins keeps the package entry; canonical dedupe
        // drops the auto twin. Package sorts to rank 4, after user auto.
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved[0]
                .path
                .file_name()
                .expect("name")
                .to_string_lossy(),
            "auto.md"
        );
        assert_eq!(resolved[1].path, shared);
        assert_eq!(resolved[1].metadata.origin, SourceOrigin::Package);
    }

    // --- find_source_info_for_path (resource-loader.ts:701-745) ---------------

    #[test]
    fn test_find_source_info_for_path_prefix_match() {
        let info = SourceInfo {
            path: PathBuf::from("/ext/skills"),
            source: "extension:extra".to_string(),
            scope: SourceScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(PathBuf::from("/ext/skills")),
        };
        let extra = vec![(PathBuf::from("/ext/skills"), info)];
        let found = find_source_info_for_path(Path::new("/ext/skills/foo/SKILL.md"), &extra, &[])
            .expect("prefix match");
        assert_eq!(found.source, "extension:extra");
        assert_eq!(found.path, PathBuf::from("/ext/skills/foo/SKILL.md"));

        assert!(find_source_info_for_path(Path::new("/other/SKILL.md"), &extra, &[]).is_none());
        assert!(find_source_info_for_path(Path::new("<inline:1>"), &extra, &[]).is_none());
    }

    // --- loader initial state -------------------------------------------------

    #[test]
    fn test_new_loader_is_empty_before_reload() {
        let tmp = TempDir::new();
        let loader = DefaultResourceLoader::new(DefaultResourceLoaderOptions::new(
            tmp.path().join("project"),
            tmp.path().join("agent"),
        ));
        assert!(!loader.is_loaded());
        assert!(loader.resources().skills.is_empty());
        assert!(loader.resources().prompts.is_empty());
        assert!(loader.resources().themes.is_empty());
        assert!(loader.resources().context_files.is_empty());
        assert!(loader.resources().diagnostics.is_empty());
        assert!(loader.resources().extensions.paths.is_empty());
        assert!(loader.resources().system_prompt.is_none());
    }

    // --- keybindings migration (migrations.ts:157-172) ------------------------

    #[test]
    fn test_migrate_keybindings_config_file_at_writes_back() {
        let tmp = TempDir::new();
        let path = tmp.path().join("keybindings.json");
        let (legacy, modern) = KEYBINDING_NAME_MIGRATIONS[0];
        write(&path, &format!("{{\"{legacy}\": \"ctrl+x\"}}\n"));

        let migrated = migrate_keybindings_config_file_at(&path).expect("migration ok");
        assert!(migrated);
        let written = std::fs::read_to_string(&path).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid json");
        assert_eq!(parsed.get(modern).and_then(Value::as_str), Some("ctrl+x"));
        assert!(parsed.get(legacy).is_none());
        assert!(
            written.ends_with("}\n"),
            "pretty-printed with trailing newline"
        );

        // Second run: nothing left to migrate.
        let again = migrate_keybindings_config_file_at(&path).expect("migration ok");
        assert!(!again);
    }

    #[test]
    fn test_migrate_keybindings_config_file_at_silent_on_malformed() {
        let tmp = TempDir::new();
        let path = tmp.path().join("keybindings.json");

        // Missing file: no-op.
        assert!(!migrate_keybindings_config_file_at(&path).expect("ok"));

        // Malformed JSON: ignored silently.
        write(&path, "{ not json");
        assert!(!migrate_keybindings_config_file_at(&path).expect("ok"));

        // Non-object JSON: ignored silently.
        write(&path, "[1,2,3]");
        assert!(!migrate_keybindings_config_file_at(&path).expect("ok"));
    }

    #[test]
    fn test_in_memory_settings_feed_scoped_arrays() {
        // The loader reads per-scope arrays through get_global_settings() /
        // get_project_settings(); pin the contract used by reload().
        let mut manager =
            SettingsManager::in_memory(Settings::new(), SettingsManagerCreateOptions::default());
        manager.set_skill_paths(vec!["skills/extra".to_string()]);
        manager
            .set_project_skill_paths(vec!["project-skills".to_string()])
            .expect("project write");
        let global = settings_string_array(&manager.get_global_settings(), "skills");
        let project = settings_string_array(&manager.get_project_settings(), "skills");
        assert_eq!(global, vec!["skills/extra".to_string()]);
        assert_eq!(project, vec!["project-skills".to_string()]);
    }
}
