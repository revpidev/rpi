//! Integration tests for `core::resource_loader` (T09) — multi-level
//! directory fixtures covering the discovery pipeline end to end:
//! global `~/.pir/agent` → project `.pir` (trust on/off) → settings paths →
//! CLI flags → packages, rank ordering, first-wins collisions, the
//! `resources_discover` hook, the extensions placeholder and the keybindings
//! migration write-back.
//!
//! Non-extension intents of upstream `test/resource-loader.test.ts` are
//! ported under their snake_case names (coding-standards §12.2).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pir::core::keybindings::KEYBINDING_NAME_MIGRATIONS;
use pir::core::resource_loader::{
    migrate_keybindings_config_file_at, DefaultResourceLoader, DefaultResourceLoaderOptions,
    DiagnosticKind, DiagnosticResourceType, PackageResource, PackageResourcePaths,
    ResourceExtensionPath, ResourceExtensionPaths,
};
use pir::core::settings_manager::{Settings, SettingsManager, SettingsManagerCreateOptions};
use pir::core::skills::{SourceInfo, SourceOrigin, SourceScope};
use pir::core::themes::REQUIRED_COLOR_KEYS;

// ---------------------------------------------------------------------------
// Fixture helpers (same pattern as `tools.rs` test_helpers::TempDir)
// ---------------------------------------------------------------------------

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
            "pir-resource-loader-it-{}-{nanos}-{id}",
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

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
}

/// A minimal valid custom theme (51 required color tokens, themes.rs).
fn theme_json(name: &str) -> String {
    let colors: serde_json::Map<String, serde_json::Value> = REQUIRED_COLOR_KEYS
        .iter()
        .map(|key| {
            (
                key.to_string(),
                serde_json::Value::String("#000000".to_string()),
            )
        })
        .collect();
    serde_json::json!({ "name": name, "colors": serde_json::Value::Object(colors) }).to_string()
}

struct Fixture {
    _tmp: TempDir,
    cwd: PathBuf,
    agent_dir: PathBuf,
    home_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new();
        let cwd = tmp.path().join("project");
        let agent_dir = tmp.path().join("agent");
        let home_dir = tmp.path().join("home");
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        std::fs::create_dir_all(&agent_dir).expect("mkdir agent dir");
        std::fs::create_dir_all(&home_dir).expect("mkdir home dir");
        Fixture {
            _tmp: tmp,
            cwd,
            agent_dir,
            home_dir,
        }
    }

    fn options(&self) -> DefaultResourceLoaderOptions {
        let mut options =
            DefaultResourceLoaderOptions::new(self.cwd.clone(), self.agent_dir.clone());
        options.home_dir = Some(self.home_dir.clone());
        options
    }

    fn project_dir(&self) -> PathBuf {
        self.cwd.join(".pir")
    }
}

fn extension_source_info(source: &str, path: &Path) -> SourceInfo {
    SourceInfo {
        path: path.to_path_buf(),
        source: source.to_string(),
        scope: SourceScope::Temporary,
        origin: SourceOrigin::TopLevel,
        base_dir: Some(path.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// reload — upstream describe("reload")
// ---------------------------------------------------------------------------

#[test]
fn should_initialize_with_empty_results_before_reload() {
    let fixture = Fixture::new();
    let loader = DefaultResourceLoader::new(fixture.options());

    assert!(!loader.is_loaded());
    assert!(loader.resources().extensions.paths.is_empty());
    assert!(loader.resources().skills.is_empty());
    assert!(loader.resources().prompts.is_empty());
    assert!(loader.resources().themes.is_empty());
}

#[test]
fn should_discover_skills_from_agent_dir() {
    let fixture = Fixture::new();
    write(
        &fixture.agent_dir.join("skills").join("test-skill.md"),
        &skill_md("test-skill", "A test skill"),
    );

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert!(loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "test-skill"));
}

#[test]
fn should_ignore_extra_markdown_files_in_auto_discovered_skill_dirs() {
    let fixture = Fixture::new();
    let skill_dir = fixture
        .agent_dir
        .join("skills")
        .join("pi-skills")
        .join("browser-tools");
    write(
        &skill_dir.join("SKILL.md"),
        &skill_md("browser-tools", "Browser tools"),
    );
    write(&skill_dir.join("EFFICIENCY.md"), "No frontmatter here");

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert!(loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "browser-tools"));
    assert!(!loader.skill_diagnostics().iter().any(|d| d
        .path
        .as_ref()
        .is_some_and(|p| p.to_string_lossy().ends_with("EFFICIENCY.md"))));
}

#[test]
fn should_discover_prompts_from_agent_dir() {
    let fixture = Fixture::new();
    write(
        &fixture.agent_dir.join("prompts").join("test-prompt.md"),
        "---\ndescription: A test prompt\n---\nPrompt content.",
    );

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert!(loader
        .resources()
        .prompts
        .iter()
        .any(|p| p.name == "test-prompt"));
}

#[test]
fn should_prefer_project_resources_over_user_on_name_collisions() {
    let fixture = Fixture::new();

    // Prompts: same name in user auto (rank 3) and project auto (rank 1).
    let user_prompt = fixture.agent_dir.join("prompts").join("commit.md");
    let project_prompt = fixture.project_dir().join("prompts").join("commit.md");
    write(&user_prompt, "User prompt");
    write(&project_prompt, "Project prompt");

    // Skills: same name in user auto and project auto.
    let user_skill = fixture
        .agent_dir
        .join("skills")
        .join("collision-skill")
        .join("SKILL.md");
    let project_skill = fixture
        .project_dir()
        .join("skills")
        .join("collision-skill")
        .join("SKILL.md");
    write(&user_skill, &skill_md("collision-skill", "user"));
    write(&project_skill, &skill_md("collision-skill", "project"));

    // Themes: same name in user auto and project auto.
    let user_theme = fixture.agent_dir.join("themes").join("collision.json");
    let project_theme = fixture.project_dir().join("themes").join("collision.json");
    write(&user_theme, &theme_json("collision-theme"));
    write(&project_theme, &theme_json("collision-theme"));

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    let prompt = loader
        .resources()
        .prompts
        .iter()
        .find(|p| p.name == "commit")
        .expect("commit prompt");
    assert_eq!(prompt.file_path, project_prompt);

    let skill = loader
        .resources()
        .skills
        .iter()
        .find(|s| s.name == "collision-skill")
        .expect("collision-skill");
    assert_eq!(skill.file_path, project_skill);

    let theme = loader
        .resources()
        .themes
        .iter()
        .find(|t| t.name.as_deref() == Some("collision-theme"))
        .expect("collision-theme");
    assert_eq!(theme.source_path.as_deref(), Some(project_theme.as_path()));

    // First-wins also produces collision diagnostics per resource type.
    let collisions: Vec<_> = loader
        .resources()
        .diagnostics
        .iter()
        .filter(|d| d.kind == DiagnosticKind::Collision)
        .collect();
    assert!(collisions.iter().any(|d| d
        .collision
        .as_ref()
        .is_some_and(|c| c.resource_type == DiagnosticResourceType::Prompt)));
    assert!(collisions.iter().any(|d| d
        .collision
        .as_ref()
        .is_some_and(|c| c.resource_type == DiagnosticResourceType::Skill)));
    assert!(collisions.iter().any(|d| d
        .collision
        .as_ref()
        .is_some_and(|c| c.resource_type == DiagnosticResourceType::Theme)));
}

#[test]
fn should_honor_overrides_for_auto_discovered_resources() {
    let fixture = Fixture::new();

    let mut settings_manager =
        SettingsManager::in_memory(Settings::new(), SettingsManagerCreateOptions::default());
    settings_manager.set_skill_paths(vec!["-skills/skip-skill".to_string()]);
    settings_manager.set_prompt_template_paths(vec!["-prompts/skip.md".to_string()]);
    settings_manager.set_theme_paths(vec!["-themes/skip.json".to_string()]);

    write(
        &fixture
            .agent_dir
            .join("skills")
            .join("skip-skill")
            .join("SKILL.md"),
        &skill_md("skip-skill", "Skip me"),
    );
    write(
        &fixture.agent_dir.join("prompts").join("skip.md"),
        "Skip prompt",
    );
    write(
        &fixture.agent_dir.join("themes").join("skip.json"),
        &theme_json("skip-theme"),
    );

    let mut options = fixture.options();
    options.settings_manager = Some(settings_manager);
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    assert!(!loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "skip-skill"));
    assert!(!loader.resources().prompts.iter().any(|p| p.name == "skip"));
    assert!(!loader.resources().themes.iter().any(|t| t
        .source_path
        .as_ref()
        .is_some_and(|p| p.to_string_lossy().ends_with("skip.json"))));
}

#[test]
fn should_discover_agents_md_context_files() {
    let fixture = Fixture::new();
    write(
        &fixture.cwd.join("AGENTS.md"),
        "# Project Guidelines\n\nBe helpful.",
    );

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert!(loader
        .resources()
        .context_files
        .iter()
        .any(|f| f.path == fixture.cwd.join("AGENTS.md")));
}

#[test]
fn should_ignore_context_file_candidates_that_are_directories() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.cwd.join("AGENTS.md")).expect("mkdir AGENTS.md");
    write(&fixture.cwd.join("CLAUDE.md"), "Fallback instructions");

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert!(loader
        .resources()
        .context_files
        .iter()
        .any(|f| f.path == fixture.cwd.join("CLAUDE.md") && f.content == "Fallback instructions"));
}

#[test]
fn should_skip_context_files_when_no_context_files_is_true() {
    let fixture = Fixture::new();
    write(&fixture.cwd.join("AGENTS.md"), "# Project Guidelines");
    write(&fixture.cwd.join("CLAUDE.md"), "# Claude Guidelines");

    let mut options = fixture.options();
    options.no_context_files = true;
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    assert!(loader.resources().context_files.is_empty());
}

#[test]
fn should_discover_system_md_from_project_config_dir() {
    let fixture = Fixture::new();
    write(
        &fixture.project_dir().join("SYSTEM.md"),
        "You are a helpful assistant.",
    );

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert_eq!(
        loader.resources().system_prompt.as_deref(),
        Some("You are a helpful assistant.")
    );
}

#[test]
fn should_skip_project_resources_that_require_trust_when_project_is_not_trusted() {
    let fixture = Fixture::new();
    write(
        &fixture.project_dir().join("SYSTEM.md"),
        "Project system prompt.",
    );
    write(
        &fixture.agent_dir.join("SYSTEM.md"),
        "Global system prompt.",
    );
    write(&fixture.agent_dir.join("AGENTS.md"), "Global instructions");
    write(&fixture.cwd.join("AGENTS.md"), "Project instructions");
    write(
        &fixture
            .project_dir()
            .join("skills")
            .join("project-skill")
            .join("SKILL.md"),
        &skill_md("project-skill", "Project skill"),
    );
    write(
        &fixture.project_dir().join("prompts").join("project.md"),
        "Project prompt",
    );
    write(
        &fixture.project_dir().join("themes").join("project.json"),
        &theme_json("project-theme"),
    );

    let settings_manager = SettingsManager::create(
        &fixture.cwd,
        Some(&fixture.agent_dir),
        SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    let mut options = fixture.options();
    options.settings_manager = Some(settings_manager);
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    assert_eq!(
        loader.resources().system_prompt.as_deref(),
        Some("Global system prompt.")
    );
    // Context files load regardless of trust.
    assert!(loader
        .resources()
        .context_files
        .iter()
        .any(|f| f.path == fixture.agent_dir.join("AGENTS.md")));
    assert!(loader
        .resources()
        .context_files
        .iter()
        .any(|f| f.path == fixture.cwd.join("AGENTS.md")));
    assert!(!loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "project-skill"));
    assert!(!loader
        .resources()
        .prompts
        .iter()
        .any(|p| p.name == "project"));
    assert!(!loader
        .resources()
        .themes
        .iter()
        .any(|t| t.name.as_deref() == Some("project-theme")));
}

#[test]
fn should_discover_append_system_md() {
    let fixture = Fixture::new();
    write(
        &fixture.project_dir().join("APPEND_SYSTEM.md"),
        "Additional instructions.",
    );

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    assert!(loader
        .resources()
        .append_system_prompt
        .iter()
        .any(|s| s == "Additional instructions."));
}

// ---------------------------------------------------------------------------
// noSkills option — upstream describe("noSkills option")
// ---------------------------------------------------------------------------

#[test]
fn should_skip_skill_discovery_when_no_skills_is_true() {
    let fixture = Fixture::new();
    write(
        &fixture.agent_dir.join("skills").join("test-skill.md"),
        &skill_md("test-skill", "A test skill"),
    );

    let mut options = fixture.options();
    options.no_skills = true;
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    assert!(loader.resources().skills.is_empty());
}

#[test]
fn should_still_load_additional_skill_paths_when_no_skills_is_true() {
    let fixture = Fixture::new();
    let custom_dir = fixture._tmp.path().join("custom-skills");
    write(
        &custom_dir.join("custom.md"),
        &skill_md("custom", "Custom skill"),
    );

    let mut options = fixture.options();
    options.no_skills = true;
    options.additional_skill_paths = vec![custom_dir.to_string_lossy().into_owned()];
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    assert!(loader.resources().skills.iter().any(|s| s.name == "custom"));
}

// ---------------------------------------------------------------------------
// extendResources — upstream describe("extendResources")
// ---------------------------------------------------------------------------

#[test]
fn extend_resources_loads_skills_and_prompts_with_extension_metadata() {
    let fixture = Fixture::new();
    let extra_skill_dir = fixture._tmp.path().join("extra-skills").join("extra-skill");
    let skill_path = extra_skill_dir.join("SKILL.md");
    write(&skill_path, &skill_md("extra-skill", "Extra skill"));

    let extra_prompt_dir = fixture._tmp.path().join("extra-prompts");
    let prompt_path = extra_prompt_dir.join("extra.md");
    write(
        &prompt_path,
        "---\ndescription: Extra prompt\n---\nExtra prompt content",
    );

    let mut loader = DefaultResourceLoader::new(fixture.options());
    loader.reload();

    loader.extend_resources(&ResourceExtensionPaths {
        skill_paths: vec![ResourceExtensionPath {
            path: extra_skill_dir.clone(),
            source_info: extension_source_info("extension:extra", &extra_skill_dir),
        }],
        prompt_paths: vec![ResourceExtensionPath {
            path: prompt_path.clone(),
            source_info: extension_source_info("extension:extra", &extra_prompt_dir),
        }],
        theme_paths: Vec::new(),
    });

    let skill = loader
        .resources()
        .skills
        .iter()
        .find(|s| s.name == "extra-skill")
        .expect("extra-skill loaded");
    assert_eq!(skill.source_info.source, "extension:extra");
    assert_eq!(skill.source_info.path, skill_path);

    assert!(loader
        .resources()
        .prompts
        .iter()
        .any(|p| p.name == "extra" && p.file_path == prompt_path));
    // Prompt source info is exposed for the extension host (no sourceInfo on
    // the Rust PromptTemplate itself).
    assert!(loader
        .extension_prompt_source_infos()
        .iter()
        .any(|(p, _)| p == &prompt_path));
}

// ---------------------------------------------------------------------------
// Two-phase trust grouping (requirements §7.8, T14 wires the trust prompt)
// ---------------------------------------------------------------------------

#[test]
fn set_project_trusted_loads_second_phase_resources() {
    let fixture = Fixture::new();
    write(
        &fixture
            .project_dir()
            .join("skills")
            .join("phase-skill")
            .join("SKILL.md"),
        &skill_md("phase-skill", "Project skill"),
    );
    write(
        &fixture.agent_dir.join("skills").join("global-skill.md"),
        &skill_md("global-skill", "Global skill"),
    );

    let settings_manager = SettingsManager::create(
        &fixture.cwd,
        Some(&fixture.agent_dir),
        SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    let mut options = fixture.options();
    options.settings_manager = Some(settings_manager);
    let mut loader = DefaultResourceLoader::new(options);

    // Pre-trust group: only global/user resources.
    loader.reload();
    assert!(!loader.is_project_trusted());
    assert!(loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "global-skill"));
    assert!(!loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "phase-skill"));

    // Post-trust group: project resources join after trust resolves.
    loader.set_project_trusted(true);
    loader.reload();
    assert!(loader
        .resources()
        .skills
        .iter()
        .any(|s| s.name == "phase-skill"));
}

// ---------------------------------------------------------------------------
// Precedence rank: project settings (0) > project auto (1) > user settings
// (2) > user auto (3) > package (4)
// ---------------------------------------------------------------------------

#[test]
fn rank_order_prefers_project_settings_then_auto_then_user() {
    let fixture = Fixture::new();

    // Four same-named skills, one per origin.
    let project_settings_skill = fixture
        ._tmp
        .path()
        .join("ps-skills")
        .join("multi")
        .join("SKILL.md");
    let user_settings_skill = fixture
        ._tmp
        .path()
        .join("us-skills")
        .join("multi")
        .join("SKILL.md");
    write(
        &project_settings_skill,
        &skill_md("multi", "project-settings"),
    );
    write(&user_settings_skill, &skill_md("multi", "user-settings"));
    write(
        &fixture
            .project_dir()
            .join("skills")
            .join("multi")
            .join("SKILL.md"),
        &skill_md("multi", "project-auto"),
    );
    write(
        &fixture
            .agent_dir
            .join("skills")
            .join("multi")
            .join("SKILL.md"),
        &skill_md("multi", "user-auto"),
    );

    // Same-named prompts across the four origins.
    let project_settings_prompt = fixture._tmp.path().join("ps-prompts").join("ranked.md");
    let user_settings_prompt = fixture._tmp.path().join("us-prompts").join("ranked.md");
    write(&project_settings_prompt, "project-settings");
    write(&user_settings_prompt, "user-settings");
    write(
        &fixture.project_dir().join("prompts").join("ranked.md"),
        "project-auto",
    );
    write(
        &fixture.agent_dir.join("prompts").join("ranked.md"),
        "user-auto",
    );

    // Same-named themes across the four origins.
    let project_settings_theme = fixture._tmp.path().join("ps-themes").join("ranked.json");
    let user_settings_theme = fixture._tmp.path().join("us-themes").join("ranked.json");
    write(&project_settings_theme, &theme_json("ranked-theme"));
    write(&user_settings_theme, &theme_json("ranked-theme"));
    write(
        &fixture.project_dir().join("themes").join("ranked.json"),
        &theme_json("ranked-theme"),
    );
    write(
        &fixture.agent_dir.join("themes").join("ranked.json"),
        &theme_json("ranked-theme"),
    );

    let mut settings_manager =
        SettingsManager::in_memory(Settings::new(), SettingsManagerCreateOptions::default());
    settings_manager.set_skill_paths(vec![user_settings_skill
        .parent()
        .expect("parent")
        .parent()
        .expect("grandparent")
        .to_string_lossy()
        .into_owned()]);
    settings_manager
        .set_project_skill_paths(vec![project_settings_skill
            .parent()
            .expect("parent")
            .parent()
            .expect("grandparent")
            .to_string_lossy()
            .into_owned()])
        .expect("project skills write");
    settings_manager.set_prompt_template_paths(vec![user_settings_prompt
        .parent()
        .expect("parent")
        .to_string_lossy()
        .into_owned()]);
    settings_manager
        .set_project_prompt_template_paths(vec![project_settings_prompt
            .parent()
            .expect("parent")
            .to_string_lossy()
            .into_owned()])
        .expect("project prompts write");
    settings_manager.set_theme_paths(vec![user_settings_theme
        .parent()
        .expect("parent")
        .to_string_lossy()
        .into_owned()]);
    settings_manager
        .set_project_theme_paths(vec![project_settings_theme
            .parent()
            .expect("parent")
            .to_string_lossy()
            .into_owned()])
        .expect("project themes write");

    let mut options = fixture.options();
    options.settings_manager = Some(settings_manager);
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    let skill = loader
        .resources()
        .skills
        .iter()
        .find(|s| s.name == "multi")
        .expect("multi skill");
    assert_eq!(skill.description, "project-settings");

    let prompt = loader
        .resources()
        .prompts
        .iter()
        .find(|p| p.name == "ranked")
        .expect("ranked prompt");
    assert_eq!(prompt.file_path, project_settings_prompt);

    let theme = loader
        .resources()
        .themes
        .iter()
        .find(|t| t.name.as_deref() == Some("ranked-theme"))
        .expect("ranked theme");
    assert_eq!(
        theme.source_path.as_deref(),
        Some(project_settings_theme.as_path())
    );
}

// ---------------------------------------------------------------------------
// Packages input port (T14): rank 4 merge
// ---------------------------------------------------------------------------

#[test]
fn package_resources_merge_at_lowest_precedence() {
    let fixture = Fixture::new();

    // Package prompt loses a name collision against user auto (rank 3 < 4).
    let package_prompt = fixture
        ._tmp
        .path()
        .join("pkg-prompts")
        .join("shared-prompt.md");
    write(&package_prompt, "package");
    write(
        &fixture.agent_dir.join("prompts").join("shared-prompt.md"),
        "user-auto",
    );

    // Package skill wins against a CLI `--skill` path (packages merge before
    // CLI paths, resource-loader.ts:419-421).
    let package_skill_dir = fixture._tmp.path().join("pkg-skills").join("cli-vs-pkg");
    write(
        &package_skill_dir.join("SKILL.md"),
        &skill_md("cli-vs-pkg", "package"),
    );
    let cli_skill_dir = fixture._tmp.path().join("cli-skills").join("cli-vs-pkg");
    write(
        &cli_skill_dir.join("SKILL.md"),
        &skill_md("cli-vs-pkg", "cli"),
    );

    let mut options = fixture.options();
    options.package_resources = PackageResourcePaths {
        prompt_paths: vec![PackageResource {
            path: package_prompt.clone(),
            enabled: true,
            scope: SourceScope::User,
            base_dir: None,
        }],
        skill_paths: vec![PackageResource {
            path: package_skill_dir.clone(),
            enabled: true,
            scope: SourceScope::User,
            base_dir: None,
        }],
        ..PackageResourcePaths::default()
    };
    options.additional_skill_paths = vec![cli_skill_dir.to_string_lossy().into_owned()];

    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    let prompt = loader
        .resources()
        .prompts
        .iter()
        .find(|p| p.name == "shared-prompt")
        .expect("shared-prompt");
    assert_eq!(
        prompt.file_path,
        fixture.agent_dir.join("prompts").join("shared-prompt.md")
    );

    let skill = loader
        .resources()
        .skills
        .iter()
        .find(|s| s.name == "cli-vs-pkg")
        .expect("cli-vs-pkg skill");
    assert_eq!(skill.description, "package");
    assert_eq!(skill.source_info.origin, SourceOrigin::Package);
}

// ---------------------------------------------------------------------------
// Extensions placeholder + missing CLI path diagnostics
// ---------------------------------------------------------------------------

#[test]
fn missing_cli_paths_surface_diagnostics() {
    let fixture = Fixture::new();
    let missing = fixture._tmp.path().join("does-not-exist");

    let mut options = fixture.options();
    options.additional_extension_paths = vec![missing.to_string_lossy().into_owned()];
    options.additional_skill_paths = vec![missing.to_string_lossy().into_owned()];
    options.additional_prompt_template_paths = vec![missing.to_string_lossy().into_owned()];
    options.additional_theme_paths = vec![missing.to_string_lossy().into_owned()];
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    // Extensions placeholder: existence-check error with the resolved path.
    assert_eq!(loader.resources().extensions.errors.len(), 1);
    assert!(loader.resources().extensions.errors[0]
        .error
        .contains("Extension path does not exist:"));
    assert_eq!(loader.resources().extensions.errors[0].path, missing);

    // Skills/themes warn inside the loader pass, which suppresses the CLI
    // error (resource-loader.ts:425-432, 459-464 dedupe by path).
    assert!(loader
        .skill_diagnostics()
        .iter()
        .any(|d| d.kind == DiagnosticKind::Warning && d.message == "skill path does not exist"));
    assert!(!loader
        .skill_diagnostics()
        .iter()
        .any(|d| d.kind == DiagnosticKind::Error));
    assert!(loader
        .theme_diagnostics()
        .iter()
        .any(|d| d.kind == DiagnosticKind::Warning && d.message == "theme path does not exist"));
    assert!(!loader
        .theme_diagnostics()
        .iter()
        .any(|d| d.kind == DiagnosticKind::Error));

    // Prompt template loading skips missing paths silently, so the CLI error
    // fires (resource-loader.ts:440-451).
    assert!(loader
        .prompt_diagnostics()
        .iter()
        .any(|d| d.kind == DiagnosticKind::Error
            && d.message == "Prompt template path does not exist"
            && d.path.as_deref() == Some(missing.as_path())));
}

#[test]
fn existing_cli_extension_path_is_collected() {
    let fixture = Fixture::new();
    let ext_dir = fixture._tmp.path().join("ext");
    std::fs::create_dir_all(&ext_dir).expect("mkdir ext");

    let mut options = fixture.options();
    options.additional_extension_paths = vec![ext_dir.to_string_lossy().into_owned()];
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    assert_eq!(loader.resources().extensions.paths, vec![ext_dir]);
    assert!(loader.resources().extensions.errors.is_empty());
}

// ---------------------------------------------------------------------------
// Keybindings migration write-back (migrations.ts:157-172)
// ---------------------------------------------------------------------------

#[test]
fn keybindings_migration_writes_back_to_disk() {
    let tmp = TempDir::new();
    let path = tmp.path().join("keybindings.json");
    let (legacy, modern) = KEYBINDING_NAME_MIGRATIONS[0];
    write(
        &path,
        &format!("{{\n  \"{legacy}\": \"ctrl+shift+x\"\n}}\n"),
    );

    let migrated = migrate_keybindings_config_file_at(&path).expect("migration ok");
    assert!(migrated);

    let written = std::fs::read_to_string(&path).expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid json");
    assert_eq!(
        parsed.get(modern).and_then(serde_json::Value::as_str),
        Some("ctrl+shift+x")
    );
    assert!(parsed.get(legacy).is_none());
    assert!(written.ends_with("}\n"));
}
