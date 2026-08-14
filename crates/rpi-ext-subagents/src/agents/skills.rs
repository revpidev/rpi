//! Skill discovery and the `<available_skills>` system-prompt injection
//! (FR-P1-02 step-level `skill` overrides, FR-P1-07 orchestration skill).
//!
//! Port of pi-subagents `src/agents/skills.ts` @ v0.48.0 (56f97234),
//! filesystem subset: search paths are the four upstream directories
//! (project `.rpi/skills` + legacy `.agents/skills`, user agent-dir skills +
//! `~/.agents/skills`); the npm "installed package" path collectors have no
//! rpi equivalent (requirements §2.3 P2: installed-package discovery) and are
//! intentionally absent. Project sources beat user sources on name conflicts
//! (`chooseHigherPrioritySkill`, SOURCE_PRIORITY).

use std::path::{Path, PathBuf};

/// `SUBAGENT_ORCHESTRATION_SKILL` (skills.ts): children never resolve the
/// bundled orchestration skill — asking for it surfaces as missing.
pub const SUBAGENT_ORCHESTRATION_SKILL: &str = "pi-subagents";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub name: String,
    /// Absolute path of the SKILL.md (or `<name>.md`) file.
    pub path: PathBuf,
    pub description: Option<String>,
}

/// `buildSkillPaths` (skills.ts:339-360) filesystem subset.
fn skill_search_paths(cwd: &Path) -> Vec<(PathBuf, &'static str)> {
    let project_config_dir = crate::agents::discover::find_configured_project_root(cwd)
        .map(|root| crate::paths::get_project_config_dir(&root))
        .unwrap_or_else(|| cwd.join(".rpi"));
    let user_agent_skills = crate::paths::get_agent_dir().join("skills");
    let mut paths = vec![
        (project_config_dir.join("skills"), "project"),
        (cwd.join(".agents").join("skills"), "project"),
        (user_agent_skills, "user"),
    ];
    if let Some(home) = crate::paths::home_dir() {
        paths.push((home.join(".agents").join("skills"), "user"));
    }
    paths
}

/// `collectFilesystemSkills` (skills.ts:433-545) subset: one-level walk with
/// `<dir>/<name>/SKILL.md` and loose `<dir>/<name>.md` forms.
fn collect_skills(cwd: &Path) -> Vec<(String, PathBuf, &'static str)> {
    let mut entries = Vec::new();
    for (dir, source) in skill_search_paths(cwd) {
        let Ok(children) = std::fs::read_dir(&dir) else {
            continue;
        };
        for child in children.flatten() {
            let name = child.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = child.path();
            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.is_file() {
                    entries.push((name, skill_file, source));
                }
                continue;
            }
            if path.is_file() && name.to_lowercase().ends_with(".md") {
                let stem = name.trim_end_matches(".md").trim_end_matches(".MD");
                if !stem.is_empty() {
                    entries.push((stem.to_string(), path, source));
                }
            }
        }
    }
    entries
}

/// `getCachedSkills` dedupe + priority (skills.ts:555-572): project beats
/// user; first definition wins within the same source.
fn discover_skills(cwd: &Path) -> Vec<ResolvedSkill> {
    let mut by_name: std::collections::BTreeMap<String, (PathBuf, &'static str)> =
        std::collections::BTreeMap::new();
    for (name, path, source) in collect_skills(cwd) {
        match by_name.get(&name) {
            Some((_, existing)) if source_priority(existing) >= source_priority(source) => {}
            _ => {
                by_name.insert(name, (path, source));
            }
        }
    }
    by_name
        .into_iter()
        .map(|(name, (path, _))| ResolvedSkill {
            description: parse_skill_description(&path),
            name,
            path,
        })
        .collect()
}

fn source_priority(source: &str) -> u8 {
    match source {
        "project" => 2,
        "user" => 1,
        _ => 0,
    }
}

/// `parseSkillDescription` (skills.ts): the `description:` frontmatter value.
fn parse_skill_description(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let frontmatter = crate::agents::frontmatter::parse_frontmatter(&content).frontmatter;
    frontmatter
        .get("description")
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
}

/// `resolveSkills` + `resolveSkillsWithFallback` (skills.ts:622-679): names →
/// resolved skills; the orchestration skill is always reported missing;
/// resolution falls back from the chain/worktree cwd to the base cwd.
pub fn resolve_skills_with_fallback(
    skill_names: &[String],
    primary_cwd: &Path,
    fallback_cwd: Option<&Path>,
) -> (Vec<ResolvedSkill>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    let primary = discover_skills(primary_cwd);
    for name in skill_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == SUBAGENT_ORCHESTRATION_SKILL {
            missing.push(trimmed.to_string());
            continue;
        }
        match primary.iter().find(|s| s.name == trimmed) {
            Some(skill) => resolved.push(skill.clone()),
            None => missing.push(trimmed.to_string()),
        }
    }
    if missing.is_empty() {
        return (resolved, missing);
    }
    let Some(fallback_cwd) = fallback_cwd else {
        return (resolved, missing);
    };
    if fallback_cwd == primary_cwd {
        return (resolved, missing);
    }
    let fallback = discover_skills(fallback_cwd);
    let mut still_missing = Vec::new();
    for name in &missing {
        match fallback.iter().find(|s| &s.name == name) {
            Some(skill) => resolved.push(skill.clone()),
            None => still_missing.push(name.clone()),
        }
    }
    (resolved, still_missing)
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `buildSkillInjection` (skills.ts:681-703): the `<available_skills>` block
/// appended to the child system prompt.
pub fn build_skill_injection(skills: &[ResolvedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "The following configured skills are available to this subagent.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in skills {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml_text(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml_text(skill.description.as_deref().unwrap_or(""))
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml_text(&skill.path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_injection_format() {
        let skills = vec![ResolvedSkill {
            name: "review".into(),
            path: PathBuf::from("/x/review/SKILL.md"),
            description: Some("Adversarial review pass".into()),
        }];
        let injection = build_skill_injection(&skills);
        assert!(injection.starts_with("The following configured skills"));
        assert!(injection.contains("<name>review</name>"));
        assert!(injection.contains("<description>Adversarial review pass</description>"));
        assert!(injection.contains("<location>/x/review/SKILL.md</location>"));
        assert!(injection.ends_with("</available_skills>"));
        assert_eq!(build_skill_injection(&[]), "");
    }

    #[test]
    fn skill_injection_escapes_xml() {
        let skills = vec![ResolvedSkill {
            name: "a<b".into(),
            path: PathBuf::from("/x/a<b/SKILL.md"),
            description: None,
        }];
        let injection = build_skill_injection(&skills);
        assert!(injection.contains("<name>a&lt;b</name>"));
        assert!(injection.contains("<location>/x/a&lt;b/SKILL.md</location>"));
        assert!(injection.contains("<description></description>"));
    }

    #[test]
    fn discovery_prefers_project_and_walks_both_forms() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-skills-{}", std::process::id()));
        let project = dir.join("proj");
        let skills_dir = project.join(".rpi").join("skills");
        std::fs::create_dir_all(skills_dir.join("dir-skill")).unwrap();
        std::fs::write(
            skills_dir.join("dir-skill").join("SKILL.md"),
            "---\ndescription: from project\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("file-skill.md"),
            "---\ndescription: loose file\n---\nbody",
        )
        .unwrap();
        let skills = discover_skills(&project);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"dir-skill"));
        assert!(names.contains(&"file-skill"));
        assert_eq!(
            skills.iter().find(|s| s.name == "dir-skill").unwrap().description.as_deref(),
            Some("from project")
        );

        // Orchestration skill is always missing.
        let names = vec![SUBAGENT_ORCHESTRATION_SKILL.to_string()];
        let (resolved, missing) = resolve_skills_with_fallback(&names, &project, None);
        assert!(resolved.is_empty());
        assert_eq!(missing, vec![SUBAGENT_ORCHESTRATION_SKILL.to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
