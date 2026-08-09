//! Integration tests for `core::skills` (T09) — multi-level directory
//! fixtures that unit tests in `skills.rs` do not cover: end-to-end
//! discovery across the full path set, the ancestor `.agents/skills` scan
//! bound (inside/outside a git repo), rank-ordered dedupe between settings
//! and auto-discovered skills, prompt XML injection and `/skill:name`
//! expansion from on-disk fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rpi::core::skills::{
    discover_skill_paths, expand_skill_command, format_skills_for_prompt, load_skills,
    DiagnosticKind, DiscoverSkillsOptions, LoadSkillsOptions, SourceScope,
};

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
            "rpi-skills-test-{}-{nanos}-{id}",
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

fn skill_md(description: &str) -> String {
    format!("---\ndescription: {description}\n---\n\nBody of {description}.\n")
}

fn options_for(tmp: &TempDir, cwd: &Path) -> DiscoverSkillsOptions {
    DiscoverSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: tmp.path().join("agent"),
        home_dir: Some(tmp.path().join("home")),
        project_trusted: true,
        global_settings_skills: Vec::new(),
        project_settings_skills: Vec::new(),
        cli_skill_paths: Vec::new(),
    }
}

fn load_discovered(options: &DiscoverSkillsOptions) -> rpi::core::skills::LoadSkillsResult {
    let paths: Vec<PathBuf> = discover_skill_paths(options)
        .into_iter()
        .filter(|p| p.enabled)
        .map(|p| p.path)
        .collect();
    load_skills(&LoadSkillsOptions {
        cwd: options.cwd.clone(),
        agent_dir: options.agent_dir.clone(),
        skill_paths: paths,
        include_defaults: false,
    })
}

// ---------------------------------------------------------------------------
// Discovery modes
// ---------------------------------------------------------------------------

#[test]
fn skills_discovery_pir_and_agents_modes_end_to_end() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");

    // `.rpi/skills` (mode "pi"): loose root .md files count as skills, and
    // SKILL.md directories are skill roots (no recursion past them).
    write(
        &cwd.join(".rpi/skills/loose.md"),
        "---\nname: loose-pi\ndescription: pi loose\n---\n",
    );
    write(
        &cwd.join(".rpi/skills/dir-skill/SKILL.md"),
        &skill_md("pi dir"),
    );
    write(
        &cwd.join(".rpi/skills/dir-skill/nested/SKILL.md"),
        &skill_md("unreachable"),
    );
    // `.agents/skills` (mode "agents"): loose root .md files do NOT count.
    write(
        &cwd.join(".agents/skills/loose.md"),
        "---\nname: loose-agents\ndescription: agents loose\n---\n",
    );
    write(
        &cwd.join(".agents/skills/agent-skill/SKILL.md"),
        &skill_md("agents dir"),
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"loose-pi"),
        "pi mode loads loose .md: {names:?}"
    );
    assert!(names.contains(&"dir-skill"));
    assert!(names.contains(&"agent-skill"));
    assert!(
        !names.contains(&"loose-agents"),
        "agents mode ignores loose .md: {names:?}"
    );
    assert!(
        !names.contains(&"nested"),
        "no recursion past a skill root: {names:?}"
    );
}

#[test]
fn skills_discovery_respects_fdignore_chain() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    write(&cwd.join(".rpi/skills/.fdignore"), "hidden-skill/\n");
    write(
        &cwd.join(".rpi/skills/hidden-skill/SKILL.md"),
        &skill_md("hidden"),
    );
    write(
        &cwd.join(".rpi/skills/visible/SKILL.md"),
        &skill_md("visible"),
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["visible"]);
}

// ---------------------------------------------------------------------------
// Rank + dedupe
// ---------------------------------------------------------------------------

#[test]
fn skills_rank_project_settings_beats_project_auto_on_name_collision() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    let settings_skill = cwd.join(".rpi/declared/dup/SKILL.md");
    let auto_skill = cwd.join(".rpi/skills/dup/SKILL.md");
    write(
        &settings_skill,
        "---\nname: dup\ndescription: from settings\n---\n",
    );
    write(&auto_skill, "---\nname: dup\ndescription: from auto\n---\n");

    let mut options = options_for(&tmp, &cwd);
    options.project_settings_skills = vec!["./declared".to_string()];

    // Discovery order: the settings entry (rank 0) precedes the auto entry
    // (rank 1) so first-wins name dedupe picks the settings skill.
    let discovered = discover_skill_paths(&options);
    assert_eq!(discovered[0].path, settings_skill);
    assert!(discovered.iter().any(|p| p.path == auto_skill));

    let result = load_discovered(&options);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "from settings");
    let collision = result
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::Collision)
        .expect("name collision diagnostic");
    assert_eq!(collision.message, "name \"dup\" collision");
    let detail = collision.collision.as_ref().expect("collision detail");
    assert_eq!(detail.winner_path, settings_skill);
    assert_eq!(detail.loser_path, auto_skill);
}

#[test]
fn skills_trust_gate_excludes_project_auto_discovery() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    write(
        &cwd.join(".rpi/skills/s/SKILL.md"),
        &skill_md("project auto"),
    );
    write(
        &cwd.join(".agents/skills/a/SKILL.md"),
        &skill_md("agents auto"),
    );
    write(
        &tmp.path().join("agent/skills/u/SKILL.md"),
        &skill_md("user auto"),
    );

    let mut options = options_for(&tmp, &cwd);
    options.project_trusted = false;
    let result = load_discovered(&options);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["u"], "untrusted project dirs are skipped");
}

// ---------------------------------------------------------------------------
// Ancestor .agents/skills scan bound
// ---------------------------------------------------------------------------

#[test]
fn skills_ancestor_scan_inside_git_repo_stops_at_repo_root() {
    let tmp = TempDir::new();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");
    let cwd = repo.join("a/b");

    write(
        &repo.join(".agents/skills/at-root/SKILL.md"),
        &skill_md("root"),
    );
    write(&repo.join("a/.agents/skills/at-a/SKILL.md"), &skill_md("a"));
    // Outside the git repo root: never scanned.
    write(
        &tmp.path().join(".agents/skills/outside/SKILL.md"),
        &skill_md("outside"),
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"at-root"),
        "repo root is inclusive: {names:?}"
    );
    assert!(names.contains(&"at-a"));
    assert!(!names.contains(&"outside"), "beyond git root: {names:?}");
}

#[test]
fn skills_ancestor_scan_without_git_repo_scans_to_filesystem_root() {
    let tmp = TempDir::new();
    // No `.git` anywhere between the cwd and `/` (temp dirs are direct
    // children of the system temp root).
    let cwd = tmp.path().join("x/y");
    write(
        &cwd.join(".agents/skills/local/SKILL.md"),
        &skill_md("local"),
    );
    write(
        &tmp.path().join(".agents/skills/top/SKILL.md"),
        &skill_md("top"),
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"local"), "{names:?}");
    assert!(
        names.contains(&"top"),
        "scan continues past non-repo ancestors to the filesystem root: {names:?}"
    );
}

#[test]
fn skills_ancestor_scan_excludes_home_agents_dir_from_project_chain() {
    let tmp = TempDir::new();
    // Home directory sits inside the ancestor chain of the cwd: its
    // `.agents/skills` must be discovered once, as the user-scope location,
    // not twice.
    let home = tmp.path().join("home");
    let cwd = home.join("proj");
    write(
        &home.join(".agents/skills/h/SKILL.md"),
        &skill_md("home skill"),
    );

    let mut options = options_for(&tmp, &cwd);
    options.home_dir = Some(home);

    let discovered = discover_skill_paths(&options);
    let matches: Vec<_> = discovered
        .iter()
        .filter(|p| p.path.ends_with("h/SKILL.md"))
        .collect();
    assert_eq!(matches.len(), 1, "no duplicate discovery");
    assert_eq!(matches[0].metadata.scope, SourceScope::User);
}

// ---------------------------------------------------------------------------
// Frontmatter semantics
// ---------------------------------------------------------------------------

#[test]
fn skills_frontmatter_field_semantics() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    let skills_dir = cwd.join(".rpi/skills");

    // Invalid name (uppercase): warning, still loaded.
    write(
        &skills_dir.join("bad-name/SKILL.md"),
        "---\nname: Bad_Name\ndescription: warned\n---\n",
    );
    // Oversized description (>1024 chars): warning, still loaded.
    let long_description = "d".repeat(1100);
    write(
        &skills_dir.join("long-desc/SKILL.md"),
        &format!("---\nname: long-desc\ndescription: {long_description}\n---\n"),
    );
    // Missing description: skill dropped with a warning.
    write(
        &skills_dir.join("no-desc/SKILL.md"),
        "---\nname: no-desc\n---\n",
    );
    // Name falls back to the parent directory name.
    write(
        &skills_dir.join("dir-fallback/SKILL.md"),
        &skill_md("fallback"),
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Bad_Name"), "loaded despite warning");
    assert!(names.contains(&"long-desc"));
    assert!(names.contains(&"dir-fallback"), "parent dir name fallback");
    assert!(
        !names.contains(&"no-desc"),
        "missing description drops skill"
    );

    let messages: Vec<&str> = result
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("invalid characters")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("description exceeds 1024 characters")),
        "{messages:?}"
    );
    assert!(messages.contains(&"description is required"));
}

// ---------------------------------------------------------------------------
// Prompt XML injection
// ---------------------------------------------------------------------------

#[test]
fn skills_xml_injection_gate_and_disable_model_invocation() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    write(
        &cwd.join(".rpi/skills/visible/SKILL.md"),
        &skill_md("visible"),
    );
    write(
        &cwd.join(".rpi/skills/hidden/SKILL.md"),
        "---\nname: hidden\ndescription: hidden skill\ndisable-model-invocation: true\n---\n",
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);
    assert_eq!(result.skills.len(), 2);

    // read tool inactive → nothing injected (system-prompt.ts:155-157).
    assert_eq!(format_skills_for_prompt(&result.skills, false), "");

    let xml = format_skills_for_prompt(&result.skills, true);
    assert!(xml.starts_with("\n\nThe following skills provide specialized instructions"));
    assert!(xml.contains("<available_skills>"));
    assert!(xml.contains("<name>visible</name>"));
    assert!(
        !xml.contains("<name>hidden</name>"),
        "disable-model-invocation skills stay out of the prompt"
    );

    // ...but the hidden skill is still invocable via /skill:hidden.
    let expanded = expand_skill_command("/skill:hidden", &result.skills).expect("expand");
    assert!(expanded.starts_with("<skill name=\"hidden\" location="));
}

#[test]
fn skills_xml_block_exact_shape_from_disk() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    let skill_file = cwd.join(".rpi/skills/exact/SKILL.md");
    write(
        &skill_file,
        "---\nname: exact\ndescription: Exact & <precise>\n---\n",
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);

    let want = [
        "",
        "",
        "The following skills provide specialized instructions for specific tasks.",
        "Use the read tool to load a skill's file when the task matches its description.",
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
        "",
        "<available_skills>",
        "  <skill>",
        "    <name>exact</name>",
        "    <description>Exact &amp; &lt;precise&gt;</description>",
        &format!("    <location>{}</location>", skill_file.display()),
        "  </skill>",
        "</available_skills>",
    ]
    .join("\n");
    assert_eq!(format_skills_for_prompt(&result.skills, true), want);
}

// ---------------------------------------------------------------------------
// /skill expansion
// ---------------------------------------------------------------------------

#[test]
fn skills_expand_command_exact_format_with_args() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    let skill_file = cwd.join(".rpi/skills/deploy/SKILL.md");
    write(
        &skill_file,
        "---\nname: deploy\ndescription: Deploy the app\n---\n\nRun the deploy pipeline.\n\nVerify the rollout.\n",
    );

    let options = options_for(&tmp, &cwd);
    let result = load_discovered(&options);

    let expanded =
        expand_skill_command("/skill:deploy to staging", &result.skills).expect("expand");
    let want = format!(
        "<skill name=\"deploy\" location=\"{}\">\nReferences are relative to {}.\n\nRun the deploy pipeline.\n\nVerify the rollout.\n</skill>\n\nto staging",
        skill_file.display(),
        skill_file
            .parent()
            .expect("skill dir")
            .display()
    );
    assert_eq!(expanded, want);

    // Unknown skill passes through untouched.
    let passthrough = expand_skill_command("/skill:nope args", &result.skills).expect("expand");
    assert_eq!(passthrough, "/skill:nope args");
}
